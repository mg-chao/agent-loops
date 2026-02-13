use agent_loops::{orchestrate, print_plan, truncate_display};
use std::sync::{Arc, Mutex};

/// The three prompts used across tests.
fn test_prompts() -> Vec<String> {
    vec![
        "What model are you?".to_string(),
        "What functions do you have?".to_string(),
        "What type of project is this?".to_string(),
    ]
}

// --- truncate_display tests ---

#[test]
fn test_truncate_short_string() {
    assert_eq!(truncate_display("hello", 60), "hello");
}

#[test]
fn test_truncate_exact_boundary() {
    let s = "a".repeat(60);
    assert_eq!(truncate_display(&s, 60), s);
}

#[test]
fn test_truncate_long_string() {
    let s = "a".repeat(100);
    let result = truncate_display(&s, 60);
    assert!(result.ends_with("..."));
    assert_eq!(result.len(), 60);
}

#[test]
fn test_truncate_zero_max() {
    assert_eq!(truncate_display("hello", 0), "...");
}

// --- orchestrate tests ---

#[tokio::test]
async fn test_orchestrate_runs_all_tasks_in_order() {
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let log_clone = Arc::clone(&log);
    let prompts = test_prompts();

    let results = orchestrate(&prompts, 1, |prompt| {
        let log = Arc::clone(&log_clone);
        async move {
            log.lock().unwrap().push(prompt);
            Ok(true)
        }
    })
    .await;

    let executed: Vec<String> = log.lock().unwrap().clone();
    assert_eq!(executed, test_prompts());
    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|(_, _, ok)| *ok));
}

#[tokio::test]
async fn test_orchestrate_cycles_with_loops() {
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let log_clone = Arc::clone(&log);
    let prompts = test_prompts();

    let results = orchestrate(&prompts, 2, |prompt| {
        let log = Arc::clone(&log_clone);
        async move {
            log.lock().unwrap().push(prompt);
            Ok(true)
        }
    })
    .await;

    let executed: Vec<String> = log.lock().unwrap().clone();
    let mut expected = test_prompts();
    expected.extend(test_prompts());
    assert_eq!(executed, expected);
    assert_eq!(results.len(), 6);
    // Verify loop/task indices
    assert_eq!(results[0], (0, 0, true));
    assert_eq!(results[1], (0, 1, true));
    assert_eq!(results[2], (0, 2, true));
    assert_eq!(results[3], (1, 0, true));
    assert_eq!(results[4], (1, 1, true));
    assert_eq!(results[5], (1, 2, true));
}

#[tokio::test]
async fn test_orchestrate_handles_failure() {
    let call_count: Arc<Mutex<usize>> = Arc::new(Mutex::new(0));
    let count_clone = Arc::clone(&call_count);
    let prompts = test_prompts();

    let results = orchestrate(&prompts, 1, |prompt| {
        let count = Arc::clone(&count_clone);
        async move {
            let mut c = count.lock().unwrap();
            *c += 1;
            // Simulate the second task failing
            if prompt == "What functions do you have?" {
                Ok(false)
            } else {
                Ok(true)
            }
        }
    })
    .await;

    // All 3 tasks should still run even if one fails
    assert_eq!(*call_count.lock().unwrap(), 3);
    assert!(results[0].2);
    assert!(!results[1].2);
    assert!(results[2].2);
}

#[tokio::test]
async fn test_orchestrate_handles_io_error() {
    let prompts = test_prompts();

    let results = orchestrate(&prompts, 1, |_prompt| async move {
        Err(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "codex not found",
        ))
    })
    .await;

    assert_eq!(results.len(), 3);
    assert!(results.iter().all(|(_, _, ok)| !ok));
}

#[tokio::test]
async fn test_orchestrate_zero_loops() {
    let prompts = test_prompts();
    let results = orchestrate(&prompts, 0, |_| async { Ok(true) }).await;
    assert!(results.is_empty());
}

#[test]
fn test_print_plan_does_not_panic() {
    let prompts = test_prompts();
    print_plan(&prompts, 2, None);
}
