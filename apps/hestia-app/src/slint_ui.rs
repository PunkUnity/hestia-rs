//! The Slint side of the UI port (`hestia_frontend::ui`).
//!
//! * [`SlintUi`] runs the controller's tasks on Slint's event loop.
//! * [`SlintView`] is the [`View`] over the generated `AppWindow`: each
//!   setter becomes a property, plain models become Slint models.
//! * [`wire`] turns the window's callbacks into the controller's intents.
//!
//! This file, the renderers (`gl_video.rs`, `gpu_render.rs`), `lib.rs` and
//! `ui/*.slint` are everything a different toolkit would replace.

use std::sync::Arc;
use std::time::Duration;

use hestia_frontend::Controller;
use hestia_frontend::ui::{self, Modifiers, Screen, SettingsForm, UiHandle, UiTask, View};
use hestia_media::DecodedFrame;
use slint::{ComponentHandle, Image, Model, ModelRc, Rgba8Pixel, SharedPixelBuffer, SharedString, VecModel};

use crate::{AppCard, AppWindow, CommandRow, HostCard, PadControl, PreflightRow};

/// Runs the controller's tasks on Slint's event loop.
pub(crate) struct SlintUi {
    weak: slint::Weak<AppWindow>,
}

impl SlintUi {
    pub(crate) fn new(window: &AppWindow) -> Self {
        Self { weak: window.as_weak() }
    }
}

impl UiHandle for SlintUi {
    fn run(&self, task: UiTask) {
        let _ = self.weak.upgrade_in_event_loop(move |window| task(&SlintView(&window)));
    }

    fn run_after(&self, delay: Duration, task: UiTask) {
        let weak = self.weak.clone();
        // Slint timers belong to the UI thread: start this one there.
        let _ = slint::invoke_from_event_loop(move || {
            slint::Timer::single_shot(delay, move || {
                if let Some(window) = weak.upgrade() {
                    task(&SlintView(&window));
                }
            });
        });
    }
}

fn strings(items: Vec<String>) -> ModelRc<SharedString> {
    ModelRc::new(VecModel::from(
        items.into_iter().map(SharedString::from).collect::<Vec<_>>(),
    ))
}

fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::new(VecModel::from(items))
}

fn art_image(art: &ui::Art) -> Image {
    Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
        &art.rgba, art.width, art.height,
    ))
}

fn host_card(card: ui::HostCard) -> HostCard {
    HostCard {
        id: card.id.into(),
        name: card.name.into(),
        address: card.address.into(),
        online: card.online,
        checking: card.checking,
        paired: card.paired,
        hermes: card.hermes,
        version: card.version.into(),
        status: card.status.into(),
        health: card.health,
    }
}

fn app_card(card: ui::AppCard) -> AppCard {
    AppCard {
        id: card.id,
        title: card.title.into(),
        has_art: card.art.is_some(),
        art: card.art.as_ref().map(art_image).unwrap_or_default(),
        running: card.running,
        hdr: card.hdr,
    }
}

pub(crate) fn pad_control(control: ui::PadControl) -> PadControl {
    PadControl {
        kind: control.kind,
        label: control.label.into(),
        x: control.x,
        y: control.y,
        w: control.w,
        h: control.h,
        pressed: control.pressed,
        knob_x: control.knob_x,
        knob_y: control.knob_y,
        up: control.up,
        down: control.down,
        left: control.left,
        right: control.right,
    }
}

/// The port's view of the window, on the UI thread.
pub(crate) struct SlintView<'a>(pub(crate) &'a AppWindow);

impl View for SlintView<'_> {
    fn set_screen(&self, screen: Screen) {
        self.0.set_screen(screen.index());
    }
    fn screen(&self) -> Screen {
        Screen::from_index(self.0.get_screen())
    }
    fn window_size(&self) -> (u32, u32) {
        let size = self.0.window().size();
        (size.width, size.height)
    }
    fn set_window_fullscreen(&self, on: bool) {
        self.0.window().set_fullscreen(on);
    }
    fn set_toast(&self, text: String) {
        self.0.set_toast(text.into());
    }
    fn toast(&self) -> String {
        self.0.get_toast().into()
    }

    fn set_starting(&self, on: bool) {
        self.0.set_starting(on);
    }
    fn set_starting_text(&self, text: String) {
        self.0.set_starting_text(text.into());
    }
    fn set_version_text(&self, text: String) {
        self.0.set_version_text(text.into());
    }
    fn set_client_id(&self, id: String) {
        self.0.set_client_id(id.into());
    }
    fn set_data_dir(&self, path: String) {
        self.0.set_data_dir(path.into());
    }

    fn set_hosts(&self, hosts: Vec<ui::HostCard>) {
        self.0.set_hosts(model(hosts.into_iter().map(host_card).collect()));
    }
    fn set_scanning(&self, on: bool) {
        self.0.set_scanning(on);
    }
    fn set_add_visible(&self, on: bool) {
        self.0.set_add_visible(on);
    }
    fn set_add_busy(&self, on: bool) {
        self.0.set_add_busy(on);
    }
    fn set_add_error(&self, text: String) {
        self.0.set_add_error(text.into());
    }
    fn set_add_address(&self, text: String) {
        self.0.set_add_address(text.into());
    }

    fn set_pair_visible(&self, on: bool) {
        self.0.set_pair_visible(on);
    }
    fn set_pair_host_name(&self, text: String) {
        self.0.set_pair_host_name(text.into());
    }
    fn set_pair_pin(&self, text: String) {
        self.0.set_pair_pin(text.into());
    }
    fn set_pair_status(&self, text: String) {
        self.0.set_pair_status(text.into());
    }
    fn set_pair_busy(&self, on: bool) {
        self.0.set_pair_busy(on);
    }
    fn set_pair_failed(&self, on: bool) {
        self.0.set_pair_failed(on);
    }
    fn set_pair_otp_mode(&self, on: bool) {
        self.0.set_pair_otp_mode(on);
    }
    fn set_otp_pin(&self, text: String) {
        self.0.set_otp_pin(text.into());
    }
    fn set_otp_passphrase(&self, text: String) {
        self.0.set_otp_passphrase(text.into());
    }

    fn set_host_name(&self, text: String) {
        self.0.set_host_name(text.into());
    }
    fn set_host_detail(&self, text: String) {
        self.0.set_host_detail(text.into());
    }
    fn set_host_chips(&self, chips: Vec<String>) {
        self.0.set_host_chips(strings(chips));
    }
    fn set_host_hermes(&self, on: bool) {
        self.0.set_host_hermes(on);
    }
    fn set_apps(&self, apps: Vec<ui::AppCard>) {
        self.0.set_apps(model(apps.into_iter().map(app_card).collect()));
    }
    fn set_app_art(&self, index: usize, app_id: i32, art: ui::Art) {
        let apps = self.0.get_apps();
        if let Some(mut card) = apps.row_data(index)
            && card.id == app_id
        {
            card.art = art_image(&art);
            card.has_art = true;
            apps.set_row_data(index, card);
        }
    }
    fn set_running_app(&self, app_id: u32) {
        let apps = self.0.get_apps();
        for i in 0..apps.row_count() {
            if let Some(mut card) = apps.row_data(i) {
                let running = app_id != 0 && card.id as u32 == app_id;
                if card.running != running {
                    card.running = running;
                    apps.set_row_data(i, card);
                }
            }
        }
    }
    fn set_apps_loading(&self, on: bool) {
        self.0.set_apps_loading(on);
    }
    fn set_app_running(&self, on: bool) {
        self.0.set_app_running(on);
    }
    fn set_apps_profile_options(&self, options: Vec<String>) {
        self.0.set_apps_profile_options(strings(options));
    }
    fn set_apps_profile_index(&self, index: i32) {
        self.0.set_apps_profile_index(index);
    }

    fn set_session_title(&self, text: String) {
        self.0.set_session_title(text.into());
    }
    fn set_session_subtitle(&self, text: String) {
        self.0.set_session_subtitle(text.into());
    }
    fn set_session_lines(&self, lines: Vec<String>) {
        self.0.set_session_lines(strings(lines));
    }
    fn set_session_busy(&self, on: bool) {
        self.0.set_session_busy(on);
    }

    fn set_report_title(&self, text: String) {
        self.0.set_report_title(text.into());
    }
    fn set_report_summary(&self, lines: Vec<String>) {
        self.0.set_report_summary(strings(lines));
    }
    fn set_report_path(&self, text: String) {
        self.0.set_report_path(text.into());
    }
    fn set_report_log_tail(&self, lines: Vec<String>) {
        self.0.set_report_log_tail(strings(lines));
    }
    fn set_report_export_label(&self, text: String) {
        self.0.set_report_export_label(text.into());
    }
    fn set_report_can_send(&self, on: bool) {
        self.0.set_report_can_send(on);
    }
    fn set_report_busy(&self, on: bool) {
        self.0.set_report_busy(on);
    }

    fn set_tools_loading(&self, on: bool) {
        self.0.set_tools_loading(on);
    }
    fn set_tools_busy(&self, on: bool) {
        self.0.set_tools_busy(on);
    }
    fn set_tools_message(&self, text: String) {
        self.0.set_tools_message(text.into());
    }
    fn set_tools_display_status(&self, text: String) {
        self.0.set_tools_display_status(text.into());
    }
    fn set_tools_display_allowed(&self, on: bool) {
        self.0.set_tools_display_allowed(on);
    }
    fn set_tools_commands(&self, commands: Vec<ui::CommandRow>) {
        let rows = commands
            .into_iter()
            .map(|c| CommandRow {
                id: c.id,
                name: c.name.into(),
                confirm: c.confirm,
                armed: c.armed,
            })
            .collect();
        self.0.set_tools_commands(model(rows));
    }
    fn set_tools_commands_allowed(&self, on: bool) {
        self.0.set_tools_commands_allowed(on);
    }
    fn set_tools_clipboard_allowed(&self, on: bool) {
        self.0.set_tools_clipboard_allowed(on);
    }
    fn set_tools_clipboard_text(&self, text: String) {
        self.0.set_tools_clipboard_text(text.into());
    }
    fn set_tools_permissions(&self, text: String) {
        self.0.set_tools_permissions(text.into());
    }
    fn set_tools_diagnostics(&self, lines: Vec<String>) {
        self.0.set_tools_diagnostics(strings(lines));
    }
    fn set_tools_status_lines(&self, lines: Vec<String>) {
        self.0.set_tools_status_lines(strings(lines));
    }
    fn set_tools_preflight(&self, rows: Vec<ui::PreflightRow>) {
        let rows = rows
            .into_iter()
            .map(|r| PreflightRow {
                level: r.level,
                title: r.title.into(),
                message: r.message.into(),
            })
            .collect();
        self.0.set_tools_preflight(model(rows));
    }

    fn show_video_frame(&self, frame: &DecodedFrame) {
        let Some(data) = frame.rgba() else { return };
        let buffer = SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(data, frame.width, frame.height);
        self.0.set_stream_frame(Image::from_rgba8(buffer));
    }
    fn clear_video(&self) {
        self.0.set_stream_frame(Image::default());
    }
    fn request_redraw(&self) {
        self.0.window().request_redraw();
    }
    fn set_stream_stats(&self, text: String) {
        self.0.set_stream_stats(text.into());
    }
    fn set_stream_status(&self, text: String) {
        self.0.set_stream_status(text.into());
    }
    fn set_stream_direct_video(&self, on: bool) {
        self.0.set_stream_direct_video(on);
    }
    fn set_stream_show_stats(&self, on: bool) {
        self.0.set_stream_show_stats(on);
    }
    fn stream_show_stats(&self) -> bool {
        self.0.get_stream_show_stats()
    }
    fn set_stream_fullscreen(&self, on: bool) {
        self.0.set_stream_fullscreen(on);
    }
    fn stream_fullscreen(&self) -> bool {
        self.0.get_stream_fullscreen()
    }
    fn set_stream_touch_mode(&self, mode: i32) {
        self.0.set_stream_touch_mode(mode);
    }
    fn set_stream_menu_open(&self, on: bool) {
        self.0.set_stream_menu_open(on);
    }
    fn set_has_touch_input(&self, on: bool) {
        self.0.set_has_touch_input(on);
    }
    fn set_stream_pad_visible(&self, on: bool) {
        self.0.set_stream_pad_visible(on);
    }
    fn set_stream_pad_controls(&self, controls: Vec<ui::PadControl>) {
        self.0
            .set_stream_pad_controls(model(controls.into_iter().map(pad_control).collect()));
    }
    fn set_stream_clipboard(&self, on: bool) {
        self.0.set_stream_clipboard(on);
    }
    fn set_stream_zoom_mode(&self, on: bool) {
        self.0.set_stream_zoom_mode(on);
    }
    fn set_stream_zoom(&self, factor: f32) {
        self.0.set_stream_zoom(factor);
    }
    fn set_stream_pan_x(&self, x: f32) {
        self.0.set_stream_pan_x(x);
    }
    fn set_stream_pan_y(&self, y: f32) {
        self.0.set_stream_pan_y(y);
    }
    fn set_stream_keyboard_open(&self, on: bool) {
        self.0.set_stream_keyboard_open(on);
    }
    fn stream_keyboard_open(&self) -> bool {
        self.0.get_stream_keyboard_open()
    }
    fn set_stream_banner(&self, text: String) {
        self.0.set_stream_banner(text.into());
    }
    fn stream_banner(&self) -> String {
        self.0.get_stream_banner().into()
    }
    fn set_stream_show_menu_button(&self, on: bool) {
        self.0.set_stream_show_menu_button(on);
    }
    fn set_stream_soft_keyboard(&self, on: bool) {
        self.0.set_stream_soft_keyboard(on);
    }
    fn set_stream_video_scale(&self, scale: i32) {
        self.0.set_stream_video_scale(scale);
    }
    fn set_stream_mouse_capture(&self, on: bool) {
        self.0.set_stream_mouse_capture(on);
    }
    fn set_stream_pointer_capture_available(&self, on: bool) {
        self.0.set_stream_pointer_capture_available(on);
    }
    fn set_pointer_capture(&self, on: bool) {
        #[cfg(not(target_os = "android"))]
        crate::desktop_input::set_pointer_capture(self.0.window(), on);
        #[cfg(target_os = "android")]
        let _ = on;
    }

    fn show_settings(&self, form: SettingsForm) {
        let w = self.0;
        w.set_cfg_resolution_options(strings(form.resolution_options));
        w.set_cfg_fps_options(strings(form.fps_options));
        w.set_cfg_codec_options(strings(form.codec_options));
        w.set_cfg_preset(form.preset);
        w.set_cfg_preset_hint(form.preset_hint.into());
        w.set_cfg_video_scale(form.video_scale);
        w.set_cfg_resolution(form.resolution);
        w.set_cfg_fps(form.fps);
        w.set_cfg_bitrate(form.bitrate_mbps);
        w.set_cfg_metered_bitrate(form.metered_bitrate_mbps);
        w.set_cfg_codec(form.codec);
        w.set_cfg_audio(form.audio);
        w.set_cfg_hdr(form.hdr);
        w.set_cfg_virtual_display(form.virtual_display);
        w.set_cfg_host_audio(form.host_audio);
        w.set_cfg_gamescope(form.gamescope);
        w.set_cfg_optimize(form.optimize);
        w.set_cfg_clipboard(form.clipboard);
        w.set_cfg_show_report(form.show_report);
        w.set_cfg_show_menu_button(form.show_menu_button);
        w.set_cfg_hardware_decoding(form.hardware_decoding);
        w.set_cfg_gpu_video(form.gpu_video);
        w.set_cfg_direct_video(form.direct_video);
        w.set_cfg_frame_pacing(form.frame_pacing);
        w.set_cfg_orientation(form.orientation);
    }
    fn settings_form(&self) -> SettingsForm {
        let w = self.0;
        let labels = |options: ModelRc<SharedString>| options.iter().map(String::from).collect();
        SettingsForm {
            resolution_options: labels(w.get_cfg_resolution_options()),
            fps_options: labels(w.get_cfg_fps_options()),
            codec_options: labels(w.get_cfg_codec_options()),
            preset: w.get_cfg_preset(),
            preset_hint: w.get_cfg_preset_hint().into(),
            video_scale: w.get_cfg_video_scale(),
            resolution: w.get_cfg_resolution(),
            fps: w.get_cfg_fps(),
            bitrate_mbps: w.get_cfg_bitrate(),
            metered_bitrate_mbps: w.get_cfg_metered_bitrate(),
            codec: w.get_cfg_codec(),
            audio: w.get_cfg_audio(),
            hdr: w.get_cfg_hdr(),
            virtual_display: w.get_cfg_virtual_display(),
            host_audio: w.get_cfg_host_audio(),
            gamescope: w.get_cfg_gamescope(),
            optimize: w.get_cfg_optimize(),
            clipboard: w.get_cfg_clipboard(),
            show_report: w.get_cfg_show_report(),
            show_menu_button: w.get_cfg_show_menu_button(),
            hardware_decoding: w.get_cfg_hardware_decoding(),
            gpu_video: w.get_cfg_gpu_video(),
            direct_video: w.get_cfg_direct_video(),
            frame_pacing: w.get_cfg_frame_pacing(),
            orientation: w.get_cfg_orientation(),
        }
    }
    fn set_cfg_video_scale(&self, scale: i32) {
        self.0.set_cfg_video_scale(scale);
    }
    fn set_cfg_profile_names(&self, names: Vec<String>) {
        self.0.set_cfg_profile_names(strings(names));
    }
    fn set_cfg_profile_pick(&self, index: i32) {
        self.0.set_cfg_profile_pick(index);
    }
    fn cfg_profile_pick(&self) -> i32 {
        self.0.get_cfg_profile_pick()
    }
}

/// The window's callbacks, as the controller's intents.
pub(crate) fn wire(ui: &AppWindow, controller: &Arc<Controller>) {
    let c = controller.clone();
    ui.on_refresh_hosts(move || c.refresh_all(true));
    let c = controller.clone();
    ui.on_open_host(move |id| c.open_host(id.to_string()));
    let c = controller.clone();
    ui.on_add_host(move |address| c.add_host(address.to_string()));

    let c = controller.clone();
    ui.on_pair_retry(move || c.restart_pairing());
    let c = controller.clone();
    ui.on_pair_use_otp(move || c.switch_to_otp());
    let c = controller.clone();
    ui.on_pair_submit_otp(move |pin, pass| c.submit_otp(pin.to_string(), pass.to_string()));
    let c = controller.clone();
    ui.on_pair_cancel(move || c.cancel_pairing());

    let c = controller.clone();
    ui.on_launch_app(move |app_id| c.launch(app_id.max(0) as u32));
    let c = controller.clone();
    ui.on_quit_app(move || c.quit_app());
    let c = controller.clone();
    ui.on_forget_host(move || c.forget_current());
    let c = controller.clone();
    ui.on_back_to_hosts(move || c.back_to_hosts());
    let c = controller.clone();
    ui.on_back_to_apps(move || c.back_to_apps());
    let c = controller.clone();
    ui.on_report_export(move || c.report_export());
    let c = controller.clone();
    ui.on_report_send_to_host(move || c.report_send_to_host());
    let c = controller.clone();
    ui.on_open_last_report(move || c.open_last_report());
    let c = controller.clone();
    ui.on_open_app_log(move || c.open_app_log());

    let c = controller.clone();
    ui.on_stream_key(move |text, pressed, repeat, ctrl, alt, shift, meta| {
        let modifiers = Modifiers { shift, ctrl, alt, meta };
        c.stream_key(&text, pressed, repeat, modifiers);
    });
    let c = controller.clone();
    ui.on_stream_pointer(move |kind, button, x, y, w, h, finger, sx, sy| {
        c.stream_pointer(kind, button, (x, y), (w, h), finger, (sx, sy))
    });
    let c = controller.clone();
    ui.on_stream_toggle_pad(move || c.toggle_pad());
    let c = controller.clone();
    ui.on_stream_clipboard_send(move || c.stream_clipboard_send());
    let c = controller.clone();
    ui.on_stream_clipboard_fetch(move || c.stream_clipboard_fetch());
    let c = controller.clone();
    ui.on_stream_toggle_zoom(move || c.toggle_zoom());
    let c = controller.clone();
    let window = ui.as_weak();
    ui.on_stream_view_size(move |w, h| {
        let scale = window.upgrade().map_or(1.0, |ui| ui.window().scale_factor());
        c.stream_view_size(w, h, scale)
    });
    let c = controller.clone();
    ui.on_stream_input_focus(move |on| c.stream_input_focus(on));
    let c = controller.clone();
    ui.on_stream_toggle_mouse_capture(move || c.toggle_mouse_capture());
    let c = controller.clone();
    ui.on_settings_custom_resolution(move |text| c.custom_resolution(&text));
    let c = controller.clone();
    ui.on_stream_scroll(move |dx, dy| c.stream_scroll(dx, dy));
    let c = controller.clone();
    ui.on_stream_keyboard(move |visible| c.set_soft_keyboard(visible));
    let c = controller.clone();
    ui.on_stream_special_key(move |name| c.stream_special_key(&name));
    let c = controller.clone();
    ui.on_stream_cycle_touch_mode(move || c.cycle_touch_mode());
    let c = controller.clone();
    ui.on_stream_request_keyframe(move || c.request_keyframe());
    let c = controller.clone();
    ui.on_stream_toggle_fullscreen(move || c.toggle_fullscreen());
    let c = controller.clone();
    ui.on_stream_disconnect(move || c.stream_disconnect());
    let c = controller.clone();
    ui.on_stream_quit(move || c.stream_quit());

    let c = controller.clone();
    ui.on_open_tools(move || c.open_tools());
    let c = controller.clone();
    ui.on_tools_refresh(move || c.tools_refresh());
    let c = controller.clone();
    ui.on_tools_recover_display(move || c.tools_recover_display());
    let c = controller.clone();
    ui.on_tools_run_command(move |id| c.tools_run_command(id));
    let c = controller.clone();
    ui.on_tools_send_clipboard(move |text| c.tools_send_clipboard(text.to_string()));
    let c = controller.clone();
    ui.on_tools_fetch_clipboard(move || c.tools_fetch_clipboard());
    let c = controller.clone();
    ui.on_tools_open_web_ui(move || c.tools_open_web_ui());
    let c = controller.clone();
    ui.on_tools_host_report(move || c.tools_host_report());

    let c = controller.clone();
    ui.on_open_settings(move || c.open_settings());
    // TV remotes: whether the D-pad moves the focus between controls (form
    // screens) or stays arrow keys (grids, the stream). Slint's Android
    // backend does the moving.
    ui.on_form_navigation_changed(|on| {
        #[cfg(target_os = "android")]
        i_slint_backend_android_activity::set_dpad_focus_navigation(on);
        #[cfg(not(target_os = "android"))]
        let _ = on;
    });
    let c = controller.clone();
    ui.on_settings_save_profile(move |name| c.save_profile(&name));
    let c = controller.clone();
    ui.on_settings_load_profile(move |index| c.load_profile(index));
    let c = controller.clone();
    ui.on_settings_delete_profile(move |index| c.delete_profile(index));
    let c = controller.clone();
    ui.on_apps_profile_chosen(move |index| c.choose_host_profile(index));
    let c = controller.clone();
    ui.on_save_settings(move || c.save_settings());
    let c = controller.clone();
    ui.on_settings_preset_changed(move |index| c.apply_preset(index));
    let c = controller.clone();
    ui.on_stream_cycle_scale(move || c.cycle_video_scale());
}
