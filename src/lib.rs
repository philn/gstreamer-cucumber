use async_std::task;
use async_trait::async_trait;
use cucumber::{given, then, when, WorldInit};
use gstreamer::glib;
use gstreamer::prelude::*;
use once_cell::sync::Lazy;
use std::cmp;
use std::convert::Infallible;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;

#[cfg(feature = "validate")]
use gstreamer_validate::prelude::*;

use gstreamer as gst;

#[cfg(feature = "validate")]
use gstreamer_validate as gstvalidate;

static CAT: Lazy<gst::DebugCategory> =
    Lazy::new(|| gst::DebugCategory::new("cucumber", gst::DebugColorFlags::empty(), Some("🥒")));

#[cfg(feature = "validate")]
#[derive(Debug)]
struct Validate {
    runner: Option<gstvalidate::Runner>,
    monitor: Option<gstvalidate::Monitor>,
    validateconfig: Option<tempfile::NamedTempFile>,
}

#[derive(Debug, WorldInit)]
pub struct World {
    pipeline: Option<gst::Element>,

    #[cfg(feature = "validate")]
    validate: Validate,

    current_feature_path: Option<PathBuf>,

    /// Information that can be gathered with additional Gherkin steps for third-party scenarios.
    pub extra_data: gst::Structure,
}

impl Drop for World {
    fn drop(&mut self) {
        let _ = self.set_pipeline_state("stop".to_string());
    }
}

impl World {
    /// Main entry point for the test harness. Input is the path to a Gherkin
    /// .feature file defining the scenario to run. `extra_data` is an optional
    /// storage that will store data gathered from additional test steps.
    pub async fn run<I>(input: I, extra_data: Option<gst::Structure>)
    where
        I: AsRef<Path>,
    {
        let extra_data = Arc::new(extra_data);
        Self::cucumber()
            .max_concurrent_scenarios(1)
            .before(move |feature, _, _scenario, world| {
                let edata = extra_data.clone();
                if let Some(d) = edata.as_ref() {
                    world.extra_data = d.clone();
                }
                world.current_feature_path = feature.path.clone();

                Box::pin(async move {
                    gst::info!(CAT, "Before: {:?} {:?}", feature, world);
                })
            })
            .after(|_, _, _, _world| {
                Box::pin(async move {
                    #[cfg(feature = "validate")]
                    if let Some(world) = _world.as_ref() {
                        if let Some(runner) = &world.validate.runner {
                            let res = runner.exit(true);
                            debug_assert!(res == 0, "Reported issues: {:?}", runner.reports());
                        }
                    }
                })
            })
            .run_and_exit(input)
            .await
    }

    /// Create the pipeline based on the given GStreamer parse-launch
    /// description. This method can be implicitely called from Gherkin in cases
    /// where the pipeline being tested is static, using the `Given Pipeline is '...'` step.
    ///
    /// Alternatively this method can be called from a custom third-party Gherkin
    /// step, in cases where the pipeline to set-up depends on third-party
    /// configuration parameters.
    pub fn set_pipeline_from_description(
        &mut self,
        pipeline_description: String,
    ) -> Result<(), anyhow::Error> {
        gst::debug!(CAT, "Pipeline is: '{}'", pipeline_description);
        self.pipeline = Some(gst::parse_launch(&pipeline_description)?);
        Ok(())
    }

    /// Set the pipeline from an already created GStreamer pipeline. This can be
    /// used for dynamic pipelines, directly involving `decodebin` GStreamer
    /// elements for instance.
    pub fn set_pipeline(&mut self, pipeline: gst::Element) {
        self.pipeline = Some(pipeline);
    }

    /// Pipeline accessor, useful for interacting with the pipeline (sending
    /// events for instance) from third-party Gherin steps.
    pub fn get_pipeline(&self) -> Result<&gst::Element, anyhow::Error> {
        self.pipeline
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Pipeline not configured yet"))
    }

    /// Changes the pipeline state, supported values for `state` are `stop`,
    /// `prepare`, `pause` and `play`. When stopping we make sure emit an EOS
    /// event, ensuring all elements have handled it and cleaned up their
    /// internal state properly.
    fn set_pipeline_state(&self, state: String) -> Result<(), anyhow::Error> {
        let pipeline = self.get_pipeline()?;

        let target_state = match state.as_str() {
            "stop" => gst::State::Null,
            "prepare" => gst::State::Ready,
            "pause" => gst::State::Paused,
            "play" => gst::State::Playing,
            _ => panic!("Invalid state name: {}", state),
        };

        if target_state == gst::State::Null {
            let (_success, current, _pending) = pipeline.state(gst::ClockTime::NONE);
            if current == target_state {
                return Ok(());
            }

            // gst-validate expects the EOS event to be matched with a previous flush sequence (?).
            let flush = if cfg!(feature = "validate") {
                true
            } else {
                false
            };

            let seqnum = gst::event::Seqnum::next();
            if flush {
                pipeline.send_event(gst::event::FlushStart::new());
                pipeline.send_event(gst::event::FlushStop::builder(true).seqnum(seqnum).build());
            }

            // Send EOS event and wait until all sinks have received it.
            pipeline.send_event(gst::event::Eos::builder().seqnum(seqnum).build());

            let bus = pipeline.bus().unwrap();
            for msg in bus.iter_timed(gst::ClockTime::NONE) {
                use gst::MessageView;

                match msg.view() {
                    MessageView::Eos(..) => break,
                    MessageView::Error(err) => {
                        eprintln!(
                            "Error from {:?}: {} ({:?})",
                            err.src().map(|s| s.path_string()),
                            err.error(),
                            err.debug()
                        );
                        break;
                    }
                    _ => (),
                }
            }
        }

        pipeline
            .set_state(target_state)
            .map(|_| ())
            .map_err(|_| anyhow::anyhow!("Unable to set pipeline state"))
    }

    fn find_element_property(
        &self,
        propname: &str,
    ) -> Result<(glib::ParamSpec, glib::Object), anyhow::Error> {
        let pipeline = self.get_pipeline()?;
        let tokens = propname.split("::");
        let mut pspec = None::<glib::ParamSpec>;
        let mut obj = None::<glib::Object>;

        for token in tokens {
            match obj {
                Some(o) => {
                    debug_assert!(pspec.is_none(), "Invalid property specifier {}", propname);
                    pspec = o
                        .find_property(token)
                        .or_else(|| panic!("Couldn't find element {}", token));

                    let tmpspec = pspec.unwrap().clone();
                    if tmpspec.value_type() == glib::Object::static_type() {
                        obj = Some(o.property::<glib::Object>(token));
                        pspec = None;
                    } else {
                        obj = Some(o.clone());
                        pspec = Some(tmpspec);
                    }
                }
                None => {
                    obj = pipeline
                        .downcast_ref::<gst::Bin>()
                        .unwrap()
                        .by_name(token)
                        .map_or_else(
                            || panic!("Couldn't find element {}", token),
                            |v| Some(v.upcast()),
                        );
                }
            }
        }

        match (pspec, obj) {
            (Some(pspec), Some(obj)) => Ok((pspec, obj)),
            _ => panic!("Couldn't find object property: {}", propname),
        }
    }
}

#[async_trait(?Send)]
impl cucumber::World for World {
    type Error = Infallible;

    async fn new() -> Result<Self, Self::Error> {
        #[cfg(feature = "validate")]
        let validate = Validate {
            runner: None,
            monitor: None,
            validateconfig: None,
        };

        Ok(Self {
            pipeline: None,
            #[cfg(feature = "validate")]
            validate,
            current_feature_path: None,
            extra_data: gst::Structure::new_empty("extra"),
        })
    }
}

#[given(regex = r"Pipeline is '(.*)'$")]
fn set_pipeline(world: &mut World, pipeline: String) -> Result<(), anyhow::Error> {
    world.set_pipeline_from_description(pipeline)
}

#[when(expr = "I wait for {word} {word}")]
async fn wait(_w: &mut World, v: u64, unit: String) {
    task::sleep(match unit.to_lowercase().as_str() {
        "min" | "mins" | "minute" | "minutes" => Duration::from_secs(v * 60),
        "sec" | "secs" | "second" | "seconds" => Duration::from_secs(v),
        "ms" | "millisecond" | "milliseconds" => Duration::from_millis(v),
        "us" | "microsecond" | "microseconds" => Duration::from_micros(v),
        _ => panic!(
            "Invalid unit: {} only [min, sec, ms, us] are supported",
            unit
        ),
    })
    .await;
}

#[when(expr = "I set property {word} to {word}")]
fn set_property(w: &mut World, propname: String, value: String) -> Result<(), anyhow::Error> {
    let (pspec, obj) = w.find_element_property(&propname)?;

    gst::debug!(CAT, "Setting {}={}", propname, value);
    obj.set_property_from_str(pspec.name(), &value);
    Ok(())
}

#[then(expr = "Property {word} equals {word}")]
fn get_property(w: &mut World, propname: String, value: String) -> Result<(), anyhow::Error> {
    let (pspec, obj) = w.find_element_property(&propname)?;

    // FIXME: Use glib::Value::deserialize_with_pspec() when we can depend on 1.20 API.
    let v = glib::Value::deserialize(&value, pspec.type_()).unwrap();
    let obj_value = obj.property_value(pspec.name());
    debug_assert!(
        v.compare(&obj_value).unwrap() == cmp::Ordering::Equal,
        "{}={} != {}",
        propname,
        obj_value.serialize().unwrap(),
        v.serialize().unwrap()
    );
    Ok(())
}

#[then(expr = "Validate should not report any issue")]
#[cfg(feature = "validate")]
fn validate_no_reports(w: &mut World) -> Result<(), anyhow::Error> {
    match &w.validate.runner {
        None => debug_assert!(
            w.validate.runner.is_some(),
            "Validate hasn't been activated"
        ),
        Some(runner) => debug_assert!(
            runner.reports_count() == 0,
            "Reported issues: {}",
            runner.printf()
        ),
    }

    Ok(())
}

#[given(regex = r"The validate configuration '(.*)'$")]
#[cfg(feature = "validate")]
fn add_validate_config(w: &mut World, config: String) {
    gstvalidate::init();
    if w.validate.validateconfig.is_none() {
        w.validate.validateconfig =
            Some(tempfile::NamedTempFile::new().expect("Could not create temporary file"));
    }

    use std::io::Write;
    writeln!(w.validate.validateconfig.as_ref().unwrap(), "{}", config)
        .expect("Couldn't write temporary config");
}

#[given(expr = "Validate is activated")]
#[cfg(feature = "validate")]
fn activate_validate(w: &mut World) -> Result<(), anyhow::Error> {
    debug_assert!(
        w.validate.runner.is_none(),
        "Validate has already been activated"
    );

    if let Some(validateconfig) = w.validate.validateconfig.take() {
        let config_temp_path = validateconfig.into_temp_path();
        let path = config_temp_path
            .as_os_str()
            .to_str()
            .expect("Invalid config temporary file")
            .to_string();
        gst::debug!(CAT, "Got config: {}", &path);
        config_temp_path.keep().expect("Could not keep config");

        std::env::set_var("GST_VALIDATE_CONFIG", path);
    }

    gstvalidate::init();
    let runner = gstvalidate::Runner::new();
    let _ = w.validate.runner.insert(runner.clone());
    let pipeline = w.get_pipeline()?;
    w.validate.monitor = Some(gstvalidate::Monitor::factory_create(
        pipeline.upcast_ref::<gst::Object>(),
        &runner,
        gstvalidate::Monitor::NONE,
    ));
    Ok(())
}

#[when(expr = "I {word} the pipeline")]
pub fn set_state(w: &mut World, state: String) -> Result<(), anyhow::Error> {
    w.set_pipeline_state(state)
}

fn get_last_frame(w: &World, element_name: &str) -> Result<Option<gst::Sample>, anyhow::Error> {
    let element = w
        .get_pipeline()?
        .downcast_ref::<gst::Bin>()
        .unwrap()
        .by_name_recurse_up(element_name)
        .ok_or_else(|| anyhow::anyhow!("Could not find element: {}", element_name))?;

    get_last_frame_on_element(w, &element)
}

/// Retrieve the most recent gst::Sample from the given video sink. We assume
/// the `enable-last-sample` property is enabled on this element.
pub fn get_last_frame_on_element(
    _w: &World,
    element: &gst::Element,
) -> Result<Option<gst::Sample>, anyhow::Error> {
    let enable_last_sample = if element.find_property("enable-last-sample").is_some() {
        element.property::<bool>("enable-last-sample")
    } else {
        gst::error!(CAT, "Sink doesn't have a `enable-last-sample' property");
        false
    };

    if !enable_last_sample {
        anyhow::bail!("Property `enable-last-sample` not `true` on: {} - you need to set it when defining the pipeline", element.name());
    }

    Ok(element.property::<Option<gst::Sample>>("last-sample"))
}

#[then(expr = "The user can see a frame on {word}")]
async fn check_last_frame(w: &mut World, element_name: String) -> Result<(), anyhow::Error> {
    let _ = w.get_pipeline()?.state(gst::ClockTime::NONE);
    let timeout = Duration::from_secs(5);

    let start = SystemTime::now();
    loop {
        if get_last_frame(w, &element_name)?.is_some() {
            return Ok(());
        }

        task::sleep(Duration::from_millis(500)).await;
        if let Ok(elapsed) = start.elapsed() {
            if elapsed >= timeout {
                anyhow::bail!(
                    "Timeout reached, video sink still not pre-rolled after {} seconds",
                    timeout.as_secs()
                );
            }
        }
    }
}

// Re-export all the traits in a prelude module, so that applications
// can always "use gstreamer_cucumber::prelude::*" without getting conflicts
pub mod prelude {
    pub use crate::{get_last_frame_on_element, World};
    pub use cucumber::*;
    pub use glib;
    #[doc(hidden)]
    pub use gst::prelude::*;
    pub use gstreamer as gst;
    pub use gstreamer_video as gstvideo;
}
