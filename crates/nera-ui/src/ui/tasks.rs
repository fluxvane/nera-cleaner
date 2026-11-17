//! Background work, confirmations and notifications — one way to do each.

use super::Tone;
use gpui_kit::component::button::ButtonVariant;
use gpui_kit::component::notification::{Notification, NotificationType};
use gpui_kit::component::WindowExt as _;
use gpui_kit::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Shared, thread-safe progress state written by background work and read in
/// `render`. Cheap to clone.
#[derive(Default)]
pub struct Shared<T>(Arc<Mutex<T>>);

impl<T> Clone for Shared<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<T> Shared<T> {
    pub fn new(value: T) -> Self {
        Self(Arc::new(Mutex::new(value)))
    }

    pub fn set(&self, value: T) {
        *self.0.lock().unwrap() = value;
    }

    pub fn update(&self, f: impl FnOnce(&mut T)) {
        f(&mut self.0.lock().unwrap());
    }

    pub fn get(&self) -> T
    where
        T: Clone,
    {
        self.0.lock().unwrap().clone()
    }
}

/// Run blocking `work` on the background executor and hand its result to
/// `done` on the view. While it runs, the view re-renders every 150 ms so a
/// [`Shared`] progress value written by `work` shows up live.
///
/// Keep the returned `Task` in the view (dropping it cancels the update).
pub fn run_blocking<V: 'static, T: Send + 'static>(
    cx: &mut Context<V>,
    work: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(&mut V, T, &mut Context<V>) + 'static,
) -> Task<()> {
    let finished = Arc::new(AtomicBool::new(false));
    let flag = finished.clone();
    // Set on drop so the poll loop below also ends if `work` unwinds.
    struct Finished(Arc<AtomicBool>);
    impl Drop for Finished {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let job = cx.background_spawn(async move {
        let _finished = Finished(flag);
        work()
    });

    cx.spawn(async move |this, cx| {
        while !finished.load(Ordering::Acquire) {
            cx.background_executor()
                .timer(Duration::from_millis(150))
                .await;
            if this.update(cx, |_, cx| cx.notify()).is_err() {
                return;
            }
        }
        let result = job.await;
        let _ = this.update(cx, |view, cx| {
            done(view, result, cx);
            cx.notify();
        });
    })
}

/// Re-render the view every `every` while the view lives (live stats).
pub fn refresh_every<V: 'static>(
    cx: &mut Context<V>,
    every: Duration,
    mut tick: impl FnMut(&mut V, &mut Context<V>) + 'static,
) -> Task<()> {
    cx.spawn(async move |this, cx| loop {
        cx.background_executor().timer(every).await;
        if this
            .update(cx, |view, cx| {
                tick(view, cx);
                cx.notify();
            })
            .is_err()
        {
            return;
        }
    })
}

/// What a confirmation dialog says. `destructive` paints the OK button red.
pub struct Confirm {
    pub title: SharedString,
    pub message: SharedString,
    pub confirm_label: SharedString,
    pub destructive: bool,
}

impl Confirm {
    pub fn destructive(
        title: impl Into<SharedString>,
        message: impl Into<SharedString>,
        confirm_label: impl Into<SharedString>,
    ) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            confirm_label: confirm_label.into(),
            destructive: true,
        }
    }

    pub fn normal(
        title: impl Into<SharedString>,
        message: impl Into<SharedString>,
        confirm_label: impl Into<SharedString>,
    ) -> Self {
        Self {
            title: title.into(),
            message: message.into(),
            confirm_label: confirm_label.into(),
            destructive: false,
        }
    }
}

/// Ask before acting. When `respect_preference` is true and the user turned
/// off "Confirm before cleanup", `on_confirm` runs immediately.
pub fn confirm(
    spec: Confirm,
    respect_preference: bool,
    window: &mut Window,
    cx: &mut App,
    on_confirm: impl Fn(&mut Window, &mut App) + 'static,
) {
    if respect_preference && !crate::state::settings(cx).confirm_before_cleanup {
        on_confirm(window, cx);
        return;
    }
    let on_confirm = std::rc::Rc::new(on_confirm);
    window.open_alert_dialog(cx, move |dialog, _, _| {
        let on_confirm = on_confirm.clone();
        dialog
            .title(spec.title.clone())
            .description(spec.message.clone())
            .ok_text(spec.confirm_label.clone())
            .ok_variant(if spec.destructive {
                ButtonVariant::Danger
            } else {
                ButtonVariant::Primary
            })
            .cancel_text("Cancel")
            .show_cancel(true)
            .on_ok(move |_, window, cx| {
                on_confirm(window, cx);
                true
            })
    });
}

/// In-app toast.
pub fn notify(
    window: &mut Window,
    cx: &mut App,
    tone: Tone,
    title: impl Into<SharedString>,
    message: impl Into<SharedString>,
) {
    let kind = match tone {
        Tone::Success => NotificationType::Success,
        Tone::Warning => NotificationType::Warning,
        Tone::Danger => NotificationType::Error,
        _ => NotificationType::Info,
    };
    let note = Notification::new()
        .with_type(kind)
        .title(title.into())
        .message(message.into());
    window.push_notification(note, cx);
}

/// Notification Center banner (respects "Notify after cleanup"). macOS only
/// so far: elsewhere it does nothing and callers rely on the in-app toast.
#[cfg(not(target_os = "macos"))]
pub fn system_notify(_cx: &App, _title: &str, _body: &str) {}

/// Notification Center banner (respects "Notify after cleanup").
#[cfg(target_os = "macos")]
pub fn system_notify(cx: &App, title: &str, body: &str) {
    if !crate::state::settings(cx).notify_after_cleanup {
        return;
    }
    let script = format!(
        "display notification {} with title {}",
        applescript_str(body),
        applescript_str(title)
    );
    let _ = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .spawn();
}

#[cfg(target_os = "macos")]
fn applescript_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}
