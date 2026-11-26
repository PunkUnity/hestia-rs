# Hestia RS changes to Slint's Android backend (v1.19.0)

1. **Input hook** (`lib.rs`: `set_input_hook`; `androidwindowadapter.rs`:
   `process_inputs`). The application sees every key and motion event
   before Slint and can consume it. Slint treats joystick motion as touch
   and maps gamepad buttons to nothing, so without this a native-activity
   app can't read game controllers at all.
2. **Mouse wheel** (`androidwindowadapter.rs`: `MotionAction::Scroll`).
   Upstream has `todo!()` there, so any wheel or trackpad scroll from a
   mouse (Samsung DeX, Bluetooth mice) panicked the app. It now dispatches
   `WindowEvent::PointerScrolled`.

3. **TV remotes and controllers in the UI** (`androidwindowadapter.rs`:
   `map_key_code`; `lib.rs`: `set_dpad_focus_navigation`). A controller's
   A and B buttons map to Return and Escape (upstream: nothing), and the
   application can make the D-pad's up/down send Shift+Tab/Tab so form
   screens can be navigated with a remote.
4. **Mice are mice** (`androidwindowadapter.rs`: `is_mouse`, at the top of
   the motion-event handling). Android frames a mouse button's
   `ButtonPress`/`ButtonRelease` with `Down`/`Up`, and upstream turns those
   into a touch as well, so every click was pressed twice and a drag moved
   a finger. For a mouse pointer, `Down`, `Up` and `Cancel` are dropped
   (the button events carry the press) and `Move` is a pointer move.

5. **Text selection menu** (`androidwindowadapter.rs`, `SlintAndroidJavaHelper.java`).
   A long press can focus a text field on release and open the Android menu at
   its caret, including when the field is empty. The floating menu uses a
   nonempty anchor rectangle at the caret.
6. **Keyboard position** (`androidwindowadapter.rs`, `javahelper.rs`). Report
   the keyboard rectangle relative to the Slint window so dialogs can use it
   in Android split-screen and freeform windows.

To update, copy the new upstream directory, reapply the changes listed above,
and bump the pinned Slint version in the root `Cargo.toml` and this crate.
