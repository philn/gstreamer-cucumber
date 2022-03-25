use async_std::task;
use async_trait::async_trait;
use cucumber::{given, then, when, WorldInit};
use gst::glib;
use gst::prelude::*;
use gstvalidate::prelude::*;
use once_cell::sync::Lazy;
use std::cmp;
use std::convert::Infallible;
use std::env;
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use std::time::SystemTime;
use tempfile::NamedTempFile;

static CAT: Lazy<gst::DebugCategory> =
    Lazy::new(|| gst::DebugCategory::new("cucumber", gst::DebugColorFlags::empty(), Some("🥒")));

#[derive(Debug, WorldInit)]
pub struct World {
    pipeline: gst::Pipeline,
    runner: Option<gstvalidate::Runner>,
    monitor: Option<gstvalidate::Monitor>,

    validateconfig: Option<NamedTempFile>,

    current_feature_path: Option<PathBuf>,

    /// Information that can be gathered with additional Gherkin steps for third-party scenarios.
    pub extra_data: gst::Structure,
}

/// Main entry point for the test harness. Input is the path to a Gherkin
/// .feature file defining the scenario to run. `extra_data` is an optional
/// storage that will store data gathered from additional test steps.
impl World {
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
            .after(|_, _, _, world| {
                Box::pin(async move {
                    if let Some(world) = world.as_ref() {
                        if let Some(runner) = &world.runner {
                            let res = runner.exit(true);
                            debug_assert!(res == 0, "Reported issues: {:?}", runner.reports());
                        }
                    }
                })
            })
            .run_and_exit(input)
            .await
    }
}

#[async_trait(?Send)]
impl cucumber::World for World {
    type Error = Infallible;

    async fn new() -> Result<Self, Self::Error> {
        Ok(Self {
            pipeline: gst::Pipeline::new(None),
            runner: None,
            monitor: None,
            validateconfig: None,

            current_feature_path: None,
            extra_data: gst::Structure::new_empty("extra"),
        })
    }
}

#[given(regex = r"Pipeline is '(.*)'$")]
fn set_pipeline(world: &mut World, pipeline: String) -> Result<(), anyhow::Error> {
    gst::debug!(CAT, "Pipeline is: '{}'", pipeline);
    world
        .pipeline
        .add(&gst::parse_bin_from_description(&pipeline, false)?)
        .expect("Could not setup pipeline");

    Ok(())
}

fn find_element(pipeline: &gst::Pipeline, propname: &str) -> (glib::ParamSpec, glib::Object) {
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
                obj = pipeline.by_name(token).map_or_else(
                    || panic!("Couldn't find element {}", token),
                    |v| Some(v.upcast()),
                );
            }
        }
    }

    match (pspec, obj) {
        (Some(pspec), Some(obj)) => (pspec, obj),
        _ => panic!("Couldn't find object property: {}", propname),
    }
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
fn set_property(w: &mut World, propname: String, value: String) {
    let (pspec, obj) = find_element(&w.pipeline, &propname);

    gst::debug!(CAT, "Setting {}={}", propname, value);
    obj.set_property_from_str(pspec.name(), &value);
}

#[then(expr = "Property {word} equals {word}")]
fn get_property(w: &mut World, propname: String, value: String) {
    let (pspec, obj) = find_element(&w.pipeline, &propname);

    let v = glib::Value::deserialize_with_pspec(&value, &pspec).unwrap();
    let obj_value = obj.property_value(pspec.name());
    debug_assert!(
        v.compare(&obj_value).unwrap() == cmp::Ordering::Equal,
        "{}={} != {}",
        propname,
        obj_value.serialize().unwrap(),
        v.serialize().unwrap()
    );
}

#[then(expr = "Validate should not report any issue")]
fn validate_no_reports(w: &mut World) -> Result<(), anyhow::Error> {
    match &w.runner {
        None => debug_assert!(w.runner.is_some(), "Validate hasn't been activated"),
        Some(runner) => debug_assert!(
            runner.reports_count() == 0,
            "Reported issues: {}",
            runner.printf()
        ),
    }

    Ok(())
}

#[given(regex = r"The validate configuration '(.*)'$")]
fn add_validate_config(w: &mut World, config: String) {
    if w.validateconfig.is_none() {
        w.validateconfig = Some(NamedTempFile::new().expect("Could not create temporary file"));
    }

    writeln!(w.validateconfig.as_ref().unwrap(), "{}", config)
        .expect("Couldn't write temporary config");
}

#[given(expr = "Validate is activated")]
fn activate_validate(w: &mut World) {
    debug_assert!(w.runner.is_none(), "Validate has already been activated");

    if let Some(validateconfig) = w.validateconfig.take() {
        let config_temp_path = validateconfig.into_temp_path();
        let path = config_temp_path
            .as_os_str()
            .to_str()
            .expect("Invalid config temporary file")
            .to_string();
        gst::debug!(CAT, "Got config: {}", &path);
        config_temp_path.keep().expect("Could not keep config");

        env::set_var("GST_VALIDATE_CONFIG", path);
    }

    gstvalidate::init();
    let runner = gstvalidate::Runner::new();
    let _ = w.runner.insert(runner.clone());
    w.monitor = gstvalidate::Monitor::factory_create(
        w.pipeline.upcast_ref::<gst::Object>(),
        &runner,
        gstvalidate::Monitor::NONE,
    );
}

#[when(expr = "I {word} the pipeline")]
fn set_state(w: &mut World, state: String) {
    if let Err(err) = w.pipeline.set_state(match state.as_str() {
        "stop" => gst::State::Null,
        "prepare" => gst::State::Ready,
        "pause" => gst::State::Paused,
        "play" => gst::State::Playing,
        _ => panic!("Invalid state name: {}", state),
    }) {
        panic!("Could not set pipeline to {}: {:?}", state, err);
    }
}

fn get_last_frame(w: &World, element_name: &str) -> Result<gst::Sample, anyhow::Error> {
    let element = w
        .pipeline
        .by_name_recurse_up(element_name)
        .ok_or_else(|| anyhow::anyhow!("Could not find element: {}", element_name))?;

    let enable_last_sample = element
        .try_property::<bool>("enable-last-sample")
        .map_err(|e| {
            anyhow::anyhow!(
                "No property `enable-last-sample` on {}: {:?}",
                element_name,
                e
            )
        })?;

    if !enable_last_sample {
        return Err(anyhow::anyhow!("Property `enable-last-sample` not `true` on: {} - you need to set it when defining the pipeline", element_name));
    }

    Ok(element.property::<gst::Sample>("last-sample"))
}

#[then(expr = "The user can see a frame on {word}")]
fn check_last_frame(w: &mut World, element_name: String) -> Result<(), anyhow::Error> {
    let _ = w.pipeline.state(gst::ClockTime::NONE);

    get_last_frame(w, &element_name).map(|_| ())
}

#[then(expr = "I should see significant color {word} on {word}")]
async fn check_significant_color(
    w: &mut World,
    expected: String,
    sink_name: String,
) -> Result<(), anyhow::Error> {
    let start = SystemTime::now();
    let mut first_expected: Option<SystemTime> = None;

    // FIXME: Make this configurable?
    let timeout = Duration::from_secs(5);

    loop {
        let sample = get_last_frame(w, &sink_name)?;

        let in_info = gstvideo::VideoInfo::from_caps(sample.caps().expect("No caps in sample"))
            .unwrap_or_else(|_| panic!("Invalid video caps: {}", sample.caps().unwrap()));

        let out_info = gstvideo::VideoInfo::builder(
            gstvideo::VideoFormat::Argb,
            in_info.width(),
            in_info.height(),
        )
        .fps(in_info.fps())
        .build()
        .unwrap();

        let videoconvert = gstvideo::VideoConverter::new(&in_info, &out_info, None)
            .expect("Could not create VideoConverter");
        let frame =
            gstvideo::VideoFrame::from_buffer_readable(sample.buffer_owned().unwrap(), &in_info)
                .expect("Could not map frame");

        let buffer = gst::Buffer::with_size(out_info.size()).unwrap();
        let mut outframe = gstvideo::VideoFrame::from_buffer_writable(buffer, &out_info).unwrap();
        videoconvert.frame(&frame, &mut outframe);
        let res = match color_thief::get_palette(
            outframe.plane_data(0).unwrap(),
            color_thief::ColorFormat::Argb,
            5,
            2,
        ) {
            Err(e) => panic!("Could not extract colors: {:?}", e),
            Ok(v) => v,
        };

        let expected = expected.to_lowercase();
        for rgb in &res {
            let color = color_name::Color::similar([rgb.r, rgb.g, rgb.b]).to_lowercase();

            gst::debug!(CAT, "Got {}", color);
            if color == expected {
                if first_expected.is_none() {
                    let _ = first_expected.insert(SystemTime::now());
                }

                // Ensuring that we have the right color for 1second
                if first_expected.unwrap().elapsed().unwrap().as_millis() >= 1000 {
                    gst::info!(
                        CAT,
                        "Got right color after {}ms",
                        first_expected
                            .unwrap()
                            .duration_since(start)
                            .unwrap()
                            .as_millis()
                    );
                    return Ok(());
                }
            }
        }

        if let Ok(elapsed) = start.elapsed() {
            if elapsed >= timeout {
                return Err(anyhow::anyhow!(
                    "Timeout reached, color {} not detected on {} after {} seconds",
                    expected,
                    sink_name,
                    timeout.as_secs()
                ));
            }
        }

        // Wait for next frame.
        task::sleep(Duration::from_millis(
            1000 / (in_info.fps().numer() as u64 / in_info.fps().denom() as u64),
        ))
        .await;
    }
}
