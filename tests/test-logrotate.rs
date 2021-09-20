use futures_util::stream::StreamExt;
use linemux::MuxedLines;
use tempfile::tempdir;
use tokio::process::Command;
use tokio::time;
//use std::process::Stdio;
use std::time::Duration;

#[tokio::test]
#[ignore]
pub async fn test_logrotate() -> std::io::Result<()> {
    let line_vals_expected = vec!["foo", "bar", "baz", "qux"];

    let logdir = tempdir().unwrap();
    let logdir_path = logdir.path();
    let logfile = logdir_path.join("foo.log");

    // TODO: CARGO_MANIFEST_DIR mabye

    let mut child = Command::new("tests/test-logrotate.sh")
        .arg(logdir_path.to_str().unwrap())
        //.arg(line_vals_expected) // TODO: do csv or something)
        //.stdout(Stdio::piped())
        .spawn()
        .expect("failed to spawn");

    // TODO: pipe script stdout to logging
    // let stdout = child.stdout.take().unwrap();
    // let mut reader = BufReader::new(stdout).lines();

    let mut lines = MuxedLines::new()?;

    lines.add_file(&logfile).await?;

    let line_vals_fut = lines
        .map(|line| line.unwrap().into_inner().1)
        .take(4)
        .collect::<Vec<_>>();

    let (line_vals, status) = tokio::try_join!(
        time::timeout(Duration::from_millis(5000), line_vals_fut),
        time::timeout(Duration::from_millis(5000), child.wait()),
    ).unwrap();

    assert!(status.unwrap().success());
    assert_eq!(line_vals_expected, line_vals);

    Ok(())
}

// Ignore this (not necessary for normal application use)
// Ref: https://github.com/tokio-rs/tokio/issues/2312
use tokio_ as tokio;
