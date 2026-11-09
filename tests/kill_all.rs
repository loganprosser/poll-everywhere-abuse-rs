//! Own test binary: kill_all_browsers() kills every Chrome this process
//! launched, so it must not share a process with other browser tests.
use poll_everywhere_abuse::cdp::{Browser, LaunchOptions, find_chrome, kill_all_browsers};
use std::time::{Duration, Instant};

#[test]
fn kill_all_browsers_kills_a_live_chrome() {
    if find_chrome().is_none() {
        eprintln!("skipping: no Chrome-family browser");
        return;
    }
    let profile = std::env::temp_dir().join(format!("pollev-killall-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&profile);
    let b = Browser::launch(&LaunchOptions {
        headless: true,
        profile_dir: profile.clone(),
        chrome_path: None,
    })
    .unwrap();
    assert!(b.is_alive());
    kill_all_browsers();
    let end = Instant::now() + Duration::from_secs(5);
    while b.is_alive() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!b.is_alive(), "Chrome survived kill_all_browsers");
    drop(b); // reaps the child; must not hang or panic on an already-dead Chrome
    let _ = std::fs::remove_dir_all(&profile);
}
