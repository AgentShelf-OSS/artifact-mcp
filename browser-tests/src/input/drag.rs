// SPDX-License-Identifier: Apache-2.0
// Copyright 2026 Neil Blackman
//! [`DragMouse`]: the page's mouse with Playwright's Chromium drag-and-drop handling.
//!
//! `chrome-headless-shell` has no system drag loop, so a native drag (`draggable`, `dragstart`)
//! started by plain `Input.dispatchMouseEvent` moves never drops. Playwright's mouse therefore
//! intercepts drags (`crDragDrop.ts`): on the first move with the left button down it turns on
//! `Input.setInterceptDrags`, moves, asks every frame whether a `dragstart` ran and was not
//! cancelled, and if so takes the drag data from `Input.dragIntercepted`. From then on moves are
//! `dragOver` events and the release is a `drop` (`Input.dispatchDragEvent`). [`Page::mouse`]
//! keeps plain mouse events; use [`Page::drag_mouse`] for gestures that drag.

use serde_json::{Value, json};

use super::Mouse;
use crate::connection::lock;
use crate::{Error, Page};

/// Page-side probe, run in every frame before the first move: did that move start a drag that
/// the page did not cancel?
const SET_UP_PROBE: &str = r#"(() => {
  const key = Symbol.for("browser-tests:drag-probe");
  let started = Promise.resolve(false);
  let dragEvent = null;
  const onDrag = (event) => { dragEvent = event; };
  const onMove = () => {
    started = new Promise((resolve) => {
      window.addEventListener("dragstart", onDrag, { once: true, capture: true });
      setTimeout(() => resolve(dragEvent ? !dragEvent.defaultPrevented : false), 0);
    });
  };
  window.addEventListener("mousemove", onMove, { once: true, capture: true });
  globalThis[key] = async () => {
    const value = await started;
    window.removeEventListener("mousemove", onMove, { capture: true });
    window.removeEventListener("dragstart", onDrag, { capture: true });
    delete globalThis[key];
    return value;
  };
})()"#;

const READ_PROBE: &str = r#"(() => { const probe = globalThis[Symbol.for("browser-tests:drag-probe")]; return probe ? probe() : false; })()"#;

/// The page's mouse with drag-and-drop interception: [`Page::drag_mouse`]. It shares the
/// position and buttons of [`Page::mouse`].
pub struct DragMouse<'p> {
    page: &'p Page,
    /// The intercepted drag data while a drag is in progress.
    drag: Option<Value>,
}

impl Page {
    /// The mouse, with Playwright's drag-and-drop handling in Chromium (see [`DragMouse`]).
    pub fn drag_mouse(&self) -> DragMouse<'_> {
        DragMouse {
            page: self,
            drag: None,
        }
    }
}

impl DragMouse<'_> {
    fn mouse(&self) -> Mouse<'_> {
        Mouse { page: self.page }
    }

    /// Whether a drag is in progress.
    pub fn is_dragging(&self) -> bool {
        self.drag.is_some()
    }

    /// Move to (`x`, `y`) in `steps` intermediate moves (at least one), as [`Mouse::move_to`].
    /// With the left button down, the first move may start a drag; while one is in progress,
    /// each step is a `dragOver`.
    pub async fn move_to(&mut self, x: f64, y: f64, steps: u32) -> Result<(), Error> {
        let (from_x, from_y) = {
            let input = lock(&self.page.input);
            (input.mouse_x, input.mouse_y)
        };
        let steps = steps.max(1);
        for step in 1..=steps {
            let fraction = f64::from(step) / f64::from(steps);
            self.step(
                from_x + (x - from_x) * fraction,
                from_y + (y - from_y) * fraction,
            )
            .await?;
        }
        Ok(())
    }

    async fn step(&mut self, x: f64, y: f64) -> Result<(), Error> {
        let (buttons, modifiers) = {
            let input = lock(&self.page.input);
            (input.buttons, input.modifiers)
        };
        if let Some(data) = &self.drag {
            {
                let mut input = lock(&self.page.input);
                input.mouse_x = x;
                input.mouse_y = y;
            }
            self.page
                .command(
                    "Input.dispatchDragEvent",
                    json!({"type": "dragOver", "x": x, "y": y, "data": data, "modifiers": modifiers}),
                )
                .await?;
            return Ok(());
        }
        if buttons & 1 == 0 {
            return self.mouse().move_to(x, y, 1).await;
        }
        self.in_all_frames(SET_UP_PROBE).await;
        let mut intercepted = self.page.subscribe("Input.dragIntercepted");
        self.page
            .command("Input.setInterceptDrags", json!({"enabled": true}))
            .await?;
        let moved = self.mouse().move_to(x, y, 1).await;
        let expecting = self
            .in_all_frames(READ_PROBE)
            .await
            .contains(&Value::Bool(true));
        let data = if moved.is_ok() && expecting {
            intercepted
                .next(self.page.timeout)
                .await
                .map(|event| event.get("data").cloned().unwrap_or(Value::Null))
        } else {
            Ok(Value::Null)
        };
        self.page
            .command("Input.setInterceptDrags", json!({"enabled": false}))
            .await?;
        moved?;
        let data = data?;
        if data.is_null() {
            return Ok(());
        }
        self.page
            .command(
                "Input.dispatchDragEvent",
                json!({"type": "dragEnter", "x": x, "y": y, "data": data, "modifiers": modifiers}),
            )
            .await?;
        self.drag = Some(data);
        Ok(())
    }

    /// Evaluate `expression` in the main world of every frame; frames that fail answer `false`.
    async fn in_all_frames(&self, expression: &str) -> Vec<Value> {
        let contexts: Vec<(String, i64)> = lock(&self.page.state.contexts)
            .iter()
            .filter(|context| context.is_default)
            .map(|context| (context.session.clone(), context.id))
            .collect();
        let mut values = Vec::new();
        for (session, context) in contexts {
            let result = self
                .page
                .session_command(
                    &session,
                    "Runtime.evaluate",
                    json!({"expression": expression, "contextId": context, "awaitPromise": true, "returnByValue": true}),
                )
                .await;
            values.push(
                result
                    .ok()
                    .and_then(|result| result.pointer("/result/value").cloned())
                    .unwrap_or(Value::Bool(false)),
            );
        }
        values
    }

    /// Press the left button (nothing while a drag is in progress).
    pub async fn down(&mut self) -> Result<(), Error> {
        if self.drag.is_some() {
            return Ok(());
        }
        self.mouse().down().await
    }

    /// Release the left button: a `drop` at the current position while a drag is in progress,
    /// otherwise a plain release.
    pub async fn up(&mut self) -> Result<(), Error> {
        let Some(data) = self.drag.take() else {
            return self.mouse().up().await;
        };
        let (x, y, modifiers) = {
            let mut input = lock(&self.page.input);
            input.buttons &= !1;
            (input.mouse_x, input.mouse_y, input.modifiers)
        };
        self.page
            .command(
                "Input.dispatchDragEvent",
                json!({"type": "drop", "x": x, "y": y, "data": data, "modifiers": modifiers}),
            )
            .await?;
        Ok(())
    }
}
