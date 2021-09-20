use futures_util::stream::StreamExt;
//use futures_util::FutureExt;
use linemux::MuxedLines;
use tempfile::tempdir;
use tokio::process::Command;
use tokio::time;
use std::time::Duration;

#[tokio::test]
#[ignore]
pub async fn test_logrotate() -> std::io::Result<()> {
    let line_vals_expected = vec!["foo", "bar", "baz", "quz"];

    let logdir = tempdir().unwrap();
    let logdir_path = logdir.path();
    let logfile = logdir_path.join("foo.log");

    let logdir_path_clone = logdir_path.to_owned();
    let thandle = tokio::task::spawn_blocking(move || {
        let _logdir_path = logdir_path_clone;
    });

    // TODO: CARGO_MANIFEST_DIR mabye
    //
    let mut child = Command::new("tests/test-logrotate.sh")
        .arg(logdir_path.to_str().unwrap())
        //.arg(line_vals_expected) // TODO: do csv or something)
        .spawn()
        .expect("failed to spawn");

    // Await until the command completes
    let status = child.wait().await?;

    let mut lines = MuxedLines::new()?;

    lines.add_file(&logfile).await?;

    let line_vals_fut = lines
        .map(|line| line.unwrap().into_inner().1)
        .collect::<Vec<_>>();

    let (line_vals, _) = tokio::try_join!(
        time::timeout(Duration::from_millis(5000), line_vals_fut),
        time::timeout(Duration::from_millis(5000), thandle),
    ).unwrap();

    assert_eq!(line_vals_expected, line_vals);

    Ok(())
}

// Ignore this (not necessary for normal application use)
// Ref: https://github.com/tokio-rs/tokio/issues/2312
use tokio_ as tokio;
