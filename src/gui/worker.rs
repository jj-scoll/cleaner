//! Long jobs (scan, apply, store top matches) off the GTK main thread (§6.8).

use crate::engine::Progress;
use gtk::glib;

enum Msg<T> {
    Progress(Progress),
    Done(anyhow::Result<T>),
}

/// Runs `job` on a new thread at lowered priority. Its progress events and
/// its result come back on the main thread through `on_progress` and
/// `on_done`. If the thread dies without a result, `on_done` gets an error.
pub fn run<T: Send + 'static>(
    job: impl FnOnce(&mut dyn FnMut(Progress)) -> anyhow::Result<T> + Send + 'static,
    on_progress: impl Fn(Progress) + 'static,
    on_done: impl FnOnce(anyhow::Result<T>) + 'static,
) {
    let (tx, rx) = async_channel::unbounded::<Msg<T>>();
    std::thread::spawn(move || {
        crate::sys::lower_priority();
        let progress_tx = tx.clone();
        let result = job(&mut |p| {
            let _ = progress_tx.send_blocking(Msg::Progress(p));
        });
        let _ = tx.send_blocking(Msg::Done(result));
    });
    glib::spawn_future_local(async move {
        let mut on_done = Some(on_done);
        while let Ok(msg) = rx.recv().await {
            match msg {
                Msg::Progress(p) => on_progress(p),
                Msg::Done(result) => {
                    if let Some(f) = on_done.take() {
                        f(result);
                    }
                    break;
                }
            }
        }
        if let Some(f) = on_done.take() {
            f(Err(anyhow::anyhow!("the background job stopped unexpectedly")));
        }
    });
}
