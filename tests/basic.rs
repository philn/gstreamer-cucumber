use glib;
use gstreamer_cucumber::World;

async fn async_main() -> Result<(), anyhow::Error> {
    gst::init()?;
    World::run("tests/features/basic.feature", None).await;
    Ok(())
}

fn main() -> Result<(), anyhow::Error> {
    glib::MainContext::new().block_on(async { async_main().await })
}
