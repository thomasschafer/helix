use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use helix_event::register_hook;
use helix_term::events::EditorWillQuit;

use super::*;

fn count_editor_will_quit() -> Arc<AtomicUsize> {
    let count = Arc::new(AtomicUsize::new(0));
    let hook_count = count.clone();
    register_hook!(move |_event: &mut EditorWillQuit<'_, '_>| {
        hook_count.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
    count
}

#[tokio::test(flavor = "multi_thread")]
async fn editor_will_quit_fires_once_when_the_last_view_closes() -> anyhow::Result<()> {
    let mut app = helpers::AppBuilder::new().build()?;
    let count = count_editor_will_quit();

    test_key_sequence(&mut app, Some(":q<ret>"), None, true).await?;

    assert_eq!(count.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn editor_will_quit_waits_for_the_last_view() -> anyhow::Result<()> {
    let mut app = helpers::AppBuilder::new().build()?;
    let count = count_editor_will_quit();

    test_key_sequences(
        &mut app,
        vec![
            (
                Some(":vsplit<ret>:q<ret>"),
                Some(&|_| assert_eq!(count.load(Ordering::SeqCst), 0)),
            ),
            (Some(":q<ret>"), None),
        ],
        true,
    )
    .await?;

    assert_eq!(count.load(Ordering::SeqCst), 1);
    Ok(())
}

#[tokio::test(flavor = "multi_thread")]
async fn editor_will_quit_does_not_fire_when_quit_is_refused() -> anyhow::Result<()> {
    let mut app = helpers::AppBuilder::new().build()?;
    let count = count_editor_will_quit();

    test_key_sequence(
        &mut app,
        Some("iunsaved<esc>:q<ret>"),
        Some(&|_| assert_eq!(count.load(Ordering::SeqCst), 0)),
        false,
    )
    .await?;

    Ok(())
}
