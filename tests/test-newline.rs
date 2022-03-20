use futures_util::future::FutureExt;
use linemux::MuxedLines;
use std::time::Duration;
use tempfile::tempdir;
use tokio::process::Command;
use tokio::time;

#[tokio::test]
pub async fn test_newline() {
    let expected_line = "foo bar".to_string();

    let logdir = tempdir().unwrap();
    let logdir_path = logdir.path();
    let logfile = logdir_path.join("foo.log");

    let mut child = Command::new("tests/test-newline.sh")
        .arg(logdir_path.to_str().unwrap())
        .spawn()
        .unwrap();

    // TODO: pipe script stdout to logging

    let mut lines = MuxedLines::new().unwrap();

    lines.add_file(&logfile).await.unwrap();

    let line_val_fut = lines
        .next_line()
        .map(|line| line.unwrap().unwrap().into_inner().1);

    const TIMEOUT_2_SEC: Duration = Duration::from_millis(2000);

    let (line_val, status) = tokio::try_join!(
        time::timeout(TIMEOUT_2_SEC, line_val_fut),
        time::timeout(TIMEOUT_2_SEC, child.wait()),
    )
    .unwrap();

    assert!(status.unwrap().success());
    assert_eq!(expected_line, line_val);
}

// Ignore this (not necessary for normal application use)
// Ref: https://github.com/tokio-rs/tokio/issues/2312
use tokio_ as tokio;
