use futures_util::stream::StreamExt;
use linemux::MuxedLines;
use std::time::Duration;
use tempfile::tempdir;
use tokio::process::Command;
use tokio::time;

#[tokio::test]
#[ignore]
pub async fn test_logrotate() {
    let line_vals_expected = vec!["foo", "bar", "baz", "qux"];

    let logdir = tempdir().unwrap();
    let logdir_path = logdir.path();
    let logfile = logdir_path.join("foo.log");

    let mut child = Command::new("tests/test-logrotate.sh")
        .arg(logdir_path.to_str().unwrap())
        //.arg(line_vals_expected) // TODO: do csv or something)
        .spawn()
        .unwrap();

    // TODO: pipe script stdout to logging

    let mut lines = MuxedLines::new().unwrap();

    lines.add_file(&logfile).await.unwrap();

    let line_vals_fut = lines
        .map(|line| line.unwrap().into_inner().1)
        .take(4)
        .collect::<Vec<_>>();

    const TIMEOUT_2_SEC: Duration = Duration::from_millis(2000);

    let (line_vals, status) = tokio::try_join!(
        time::timeout(TIMEOUT_2_SEC, line_vals_fut),
        time::timeout(TIMEOUT_2_SEC, child.wait()),
    )
    .unwrap();

    assert!(status.unwrap().success());
    assert_eq!(line_vals_expected, line_vals);
}

// Ignore this (not necessary for normal application use)
// Ref: https://github.com/tokio-rs/tokio/issues/2312
use tokio_ as tokio;
