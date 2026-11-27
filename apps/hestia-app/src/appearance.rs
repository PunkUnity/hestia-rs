use std::collections::HashSet;
use std::fmt::Write;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use hestia_core::appearance::{Appearance, LaunchStyle, PaletteRole, ThemePack, read_theme_pack};
use hestia_core::store::{AppearanceTheme, Store};
use slint::{Color, ComponentHandle, Model, ModelRc, SharedString, StyledText, VecModel};

use crate::{AppWindow, InstalledThemePack, PaletteOverride, Theme, ThemeStyle, VoltagePaths, theme_file};

#[derive(Default)]
struct Catalogue {
    saved: Appearance,
    preview: Option<ThemePack>,
    reading_file: bool,
}

impl Catalogue {
    fn active_index(&self) -> usize {
        self.saved
            .active_pack
            .as_ref()
            .and_then(|id| self.saved.packs.iter().position(|pack| &pack.id == id))
            .map_or(self.saved.theme.index(), |i| AppearanceTheme::ALL.len() + i)
    }

    fn update_options(&self, ui: &AppWindow) {
        let mut names: Vec<SharedString> = ui.get_builtin_theme_names().iter().collect();
        let mut styles: Vec<ThemeStyle> = AppearanceTheme::ALL
            .into_iter()
            .map(|base| ThemeStyle {
                base: base.index() as i32,
                ..Default::default()
            })
            .collect();
        for (i, pack) in self.saved.packs.iter().chain(self.preview.iter()).enumerate() {
            let is_update_preview =
                i == self.saved.packs.len() && self.saved.packs.iter().any(|saved| saved.id == pack.id);
            names.push(if is_update_preview {
                format!("{} (preview)", pack.name).into()
            } else {
                pack.name.as_str().into()
            });
            let colors = PaletteRole::ALL
                .into_iter()
                .map(|role| {
                    pack.color(role)
                        .map_or_else(PaletteOverride::default, |[red, green, blue, alpha]| PaletteOverride {
                            supplied: true,
                            color: Color::from_argb_u8(alpha, red, green, blue),
                        })
                })
                .collect();
            styles.push(ThemeStyle {
                base: pack.base.index() as i32,
                colors: model(colors),
            });
        }
        let mut menu = names.clone();
        if !self.saved.packs.is_empty() {
            menu.push("Manage theme packs".into());
        }
        menu.push("Load Theme Pack from File".into());
        ui.set_theme_names(model(names));
        ui.set_theme_menu(model(menu));
        ui.set_theme_styles(model(styles));
    }

    fn open_manager(&self, ui: &AppWindow) {
        let rows = self
            .saved
            .packs
            .iter()
            .map(|pack| InstalledThemePack {
                id: pack.id.as_str().into(),
                name: pack.name.as_str().into(),
                active: self.saved.active_pack.as_deref() == Some(pack.id.as_str()),
                checked: false,
            })
            .collect();
        ui.set_installed_theme_packs(model(rows));
    }

    fn remove(&mut self, ids: &HashSet<&str>, draft: usize, store: &Store) -> std::io::Result<usize> {
        let builtin = AppearanceTheme::ALL.len();
        let draft_pack = draft.checked_sub(builtin).and_then(|i| {
            if i == self.saved.packs.len() {
                self.preview.as_ref().map(|pack| (pack, true))
            } else {
                self.saved.packs.get(i).map(|pack| (pack, false))
            }
        });
        let mut saved = self.saved.clone();
        saved.packs.retain(|pack| !ids.contains(pack.id.as_str()));
        if saved.packs.len() == self.saved.packs.len() {
            return Ok(draft);
        }
        if saved.active_pack.as_deref().is_some_and(|id| ids.contains(id)) {
            saved.active_pack = None;
        }
        let next_draft = draft_pack.map_or(draft, |(pack, staged)| {
            if ids.contains(pack.id.as_str()) {
                pack.base.index()
            } else if staged {
                builtin + saved.packs.len()
            } else {
                builtin
                    + saved
                        .packs
                        .iter()
                        .position(|entry| entry.id == pack.id)
                        .expect("a surviving pack retains its ID")
            }
        });
        store.save_appearance(&saved)?;
        self.saved = saved;
        if self.preview.as_ref().is_some_and(|pack| ids.contains(pack.id.as_str())) {
            self.preview = None;
        }
        Ok(next_draft)
    }

    fn save(&mut self, index: usize, style: LaunchStyle, store: &Store) -> std::io::Result<i32> {
        let mut saved = self.saved.clone();
        saved.launch_style = style;
        if let Some(base) = AppearanceTheme::ALL.get(index) {
            saved.theme = *base;
            saved.active_pack = None;
        } else {
            let pack_index = index - AppearanceTheme::ALL.len();
            let pack = if pack_index == saved.packs.len() {
                self.preview.as_ref()
            } else {
                saved.packs.get(pack_index)
            }
            .ok_or_else(|| std::io::Error::other("theme selection no longer exists"))?
            .clone();
            saved.theme = pack.base;
            saved.active_pack = Some(pack.id.clone());
            if let Some(existing) = saved.packs.iter_mut().find(|existing| existing.id == pack.id) {
                *existing = pack;
            } else {
                saved.packs.push(pack);
            }
        }
        store.save_appearance(&saved)?;
        self.saved = saved;
        self.preview = None;
        Ok(self.active_index() as i32)
    }
}

fn model<T: Clone + 'static>(items: Vec<T>) -> ModelRc<T> {
    ModelRc::from(Rc::new(VecModel::from(items)))
}

pub(crate) fn install(ui: &AppWindow, data_dir: PathBuf, picker: theme_file::Picker) {
    ui.global::<Theme>().on_spectrum_heading(spectrum_heading);
    ui.global::<Theme>().on_voltage_paths(voltage_paths);
    let saved = Store::open(&data_dir)
        .and_then(|store| store.load_appearance())
        .unwrap_or_else(|error| {
            log::warn!("could not load appearance: {error}");
            Appearance::default()
        });
    let mut state = Catalogue {
        saved,
        ..Default::default()
    };
    state.saved.packs.retain(|pack| pack.validate().is_ok());
    state.update_options(ui);
    ui.set_saved_theme(state.active_index() as i32);
    ui.set_saved_launch_style(state.saved.launch_style.index() as i32);
    let state = Arc::new(Mutex::new(state));

    let window = ui.as_weak();
    let catalogue = state.clone();
    let save_dir = data_dir.clone();
    ui.on_save_theme(move |index, launch| {
        let Some(&style) = LaunchStyle::ALL.get(launch as usize) else {
            return -1;
        };
        let Some(window) = window.upgrade() else { return -1 };
        let mut catalogue = catalogue.lock().unwrap();
        match Store::open(&save_dir).and_then(|store| catalogue.save(index as usize, style, &store)) {
            Ok(index) => {
                catalogue.update_options(&window);
                index
            }
            Err(error) => {
                log::warn!("could not save appearance: {error}");
                -1
            }
        }
    });

    let window = ui.as_weak();
    let catalogue = state.clone();
    ui.on_open_theme_manager(move || {
        if let Some(window) = window.upgrade() {
            catalogue.lock().unwrap().open_manager(&window);
        }
    });

    let window = ui.as_weak();
    let catalogue = state.clone();
    ui.on_remove_theme_packs(move |draft| {
        let Some(window) = window.upgrade() else { return -1 };
        let selected: Vec<_> = window
            .get_installed_theme_packs()
            .iter()
            .filter(|pack| pack.checked)
            .map(|pack| pack.id)
            .collect();
        let ids = selected.iter().map(|id| id.as_str()).collect();
        let mut catalogue = catalogue.lock().unwrap();
        let had_preview = catalogue.preview.is_some();
        match Store::open(&data_dir).and_then(|store| catalogue.remove(&ids, draft as usize, &store)) {
            Ok(index) => {
                catalogue.update_options(&window);
                window.set_saved_theme(catalogue.active_index() as i32);
                if had_preview && catalogue.preview.is_none() {
                    window.set_theme_import_message("".into());
                }
                index as i32
            }
            Err(error) => {
                log::warn!("could not remove theme packs: {error}");
                -1
            }
        }
    });

    let window = ui.as_weak();
    let catalogue = state.clone();
    ui.on_discard_theme_preview(move || {
        let mut catalogue = catalogue.lock().unwrap();
        if catalogue.preview.take().is_some()
            && let Some(window) = window.upgrade()
        {
            catalogue.update_options(&window);
        }
    });

    let window = ui.as_weak();
    ui.on_import_theme(move |session| {
        {
            let mut catalogue = state.lock().unwrap();
            if catalogue.reading_file {
                return;
            }
            catalogue.reading_file = true;
        }
        let state = state.clone();
        let window = window.clone();
        picker.open(Box::new(move |result| {
            let result = result.and_then(|file| file.map(|bytes| read_theme_pack(&bytes)).transpose());
            let _ = window.upgrade_in_event_loop(move |ui| {
                let mut catalogue = state.lock().unwrap();
                catalogue.reading_file = false;
                if ui.get_screen() != 3 || ui.get_appearance_session() != session {
                    return;
                }
                match result {
                    Ok(Some(pack)) => {
                        catalogue.preview = Some(pack);
                        catalogue.update_options(&ui);
                        ui.invoke_preview_theme((AppearanceTheme::ALL.len() + catalogue.saved.packs.len()) as i32);
                        ui.set_theme_import_message("Theme pack preview. Save to keep it.".into());
                    }
                    Ok(None) => {}
                    Err(error) => ui.set_theme_import_message(error.into()),
                }
            });
        }));
    });
}

pub(crate) fn voltage_paths(width: f32, height: f32, radius: f32, tick: i32) -> VoltagePaths {
    let radius = radius.min(width / 4.0).min(height / 4.0);
    let perimeter = 2.0 * (width + height - 4.0 * radius) + std::f32::consts::TAU * radius;
    if perimeter <= 0.0 {
        return VoltagePaths::default();
    }
    let time = tick as f32 / 30.0;
    let mut arc = String::new();
    let mut forks = String::new();
    for bolt in 0..2 {
        let phase = bolt as f32 * 2.3;
        let head = time * 220.0 + perimeter * bolt as f32 / 2.0;
        let length = (perimeter * 0.27).min(164.0);
        let point = |step: f32, reach: f32| {
            let angle = step * 1.14 - time * 9.0 + phase;
            // The hot strand and its branches cross from inside to outside
            // the panel edge as they wind around it.
            let bend = angle.sin() * 5.0 + (step * 2.7 + time * 18.0 + phase).sin() * 3.0;
            edge_point(
                width,
                height,
                radius,
                (head - length * step / 16.0).rem_euclid(perimeter),
                bend + reach,
            )
        };
        for step in 0..=16 {
            let (x, y) = point(step as f32, 0.0);
            let command = if step == 0 { 'M' } else { 'L' };
            let _ = write!(arc, "{command}{x:.1} {y:.1} ");
        }
        for step in [2.0_f32, 6.0, 11.0] {
            let reach = (step * 2.1 + time * 9.0 + phase).sin() * 12.0;
            let (x1, y1) = point(step, 0.0);
            let (x2, y2) = point(step + 0.8, reach);
            let (x3, y3) = point(step + 2.2, reach * 0.65);
            let _ = write!(forks, "M{x1:.1} {y1:.1} L{x2:.1} {y2:.1} L{x3:.1} {y3:.1} ");
        }
    }
    VoltagePaths {
        arc: arc.into(),
        forks: forks.into(),
    }
}

fn edge_point(width: f32, height: f32, radius: f32, mut distance: f32, offset: f32) -> (f32, f32) {
    let turn = std::f32::consts::FRAC_PI_2 * radius;
    let corners = [
        (width - radius, radius, 1.0, 0.0, width - 2.0 * radius),
        (width - radius, height - radius, 0.0, 1.0, height - 2.0 * radius),
        (radius, height - radius, -1.0, 0.0, width - 2.0 * radius),
        (radius, radius, 0.0, -1.0, height - 2.0 * radius),
    ];
    for (x, y, dx, dy, straight) in corners {
        if distance < straight {
            let along = distance - straight;
            return (
                x + dx * along + dy * (radius + offset),
                y + dy * along - dx * (radius + offset),
            );
        }
        distance -= straight;
        if distance < turn {
            let (sin, cos) = (distance / radius).sin_cos();
            let reach = radius + offset;
            return (x + (dy * cos + dx * sin) * reach, y + (dy * sin - dx * cos) * reach);
        }
        distance -= turn;
    }
    (radius, -offset)
}

pub(crate) fn spectrum_heading(title: SharedString, phase: i32) -> StyledText {
    const COLORS: [[usize; 3]; 7] = [
        [255, 116, 157],
        [255, 209, 113],
        [122, 251, 184],
        [114, 230, 255],
        [191, 156, 255],
        [255, 133, 239],
        [255, 116, 157],
    ];
    const CYCLE: usize = 96;
    let phase = phase.rem_euclid(CYCLE as i32) as usize;
    let breath = 0.90 + 0.10 * (phase as f32 * 2.0 * std::f32::consts::TAU / CYCLE as f32).cos();
    let steps = title.chars().count().saturating_sub(1).max(1);
    let mut markup = String::from("**");
    for (i, letter) in title.chars().enumerate() {
        let offset = i * 5 * CYCLE / (steps * (COLORS.len() - 1));
        let position = (offset + CYCLE - phase) % CYCLE * (COLORS.len() - 1);
        let stop = position / CYCLE;
        let mix = position % CYCLE;
        let [r, g, b] = [0, 1, 2].map(|k| {
            let value = (COLORS[stop][k] * (CYCLE - mix) + COLORS[stop + 1][k] * mix) as f32 / CYCLE as f32;
            (value * breath).round() as u8
        });
        // Numeric entities keep title characters separate from the markup.
        let _ = write!(
            markup,
            "<font color='#{r:02x}{g:02x}{b:02x}'>&#{};</font>",
            u32::from(letter)
        );
    }
    markup.push_str("**");
    StyledText::from_markdown(&markup).unwrap_or_else(|_| StyledText::from_plain_text(&title))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removing_a_pack_preserves_surviving_selection_and_changes_state_only_after_save() {
        let dir = std::env::temp_dir().join(format!("hestia-theme-remove-{}", std::process::id()));
        let store = Store::open(&dir).unwrap();
        let harbor = read_theme_pack(include_bytes!("../../../tools/theme-packs/harbor.zip")).unwrap();
        let blue = read_theme_pack(include_bytes!("../../../tools/theme-packs/blue.zip")).unwrap();
        let mut catalogue = Catalogue {
            saved: Appearance {
                theme: blue.base,
                active_pack: Some(blue.id.clone()),
                packs: vec![harbor.clone(), blue.clone()],
                launch_style: LaunchStyle::Orbit,
            },
            preview: Some(harbor.clone()),
            ..Default::default()
        };
        store.save_appearance(&catalogue.saved).unwrap();
        let first_pack = AppearanceTheme::ALL.len();
        let selected = HashSet::from([harbor.id.as_str()]);
        let obstruction = dir.join(".appearance.json.tmp");
        std::fs::create_dir(&obstruction).unwrap();
        assert!(catalogue.remove(&selected, first_pack + 1, &store).is_err());
        assert_eq!(catalogue.active_index(), first_pack + 1);
        assert!(catalogue.preview.is_some());
        std::fs::remove_dir(obstruction).unwrap();

        assert_eq!(catalogue.remove(&selected, first_pack + 1, &store).unwrap(), first_pack);
        assert_eq!(catalogue.active_index(), first_pack);
        assert!(catalogue.preview.is_none());
        let saved = Store::open(&dir).unwrap().load_appearance().unwrap();
        assert_eq!(saved.active_pack.as_deref(), Some(blue.id.as_str()));
        assert_eq!(saved.packs.len(), 1);
        assert_eq!(saved.launch_style, LaunchStyle::Orbit);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(all(target_os = "linux", debug_assertions))]
    #[test]
    fn manager_cancels_and_removes_imported_packs_with_a_remote() {
        use i_slint_backend_testing::ElementHandle;
        use slint::platform::{Key, PointerEventButton, WindowEvent};
        use std::time::Duration;

        i_slint_backend_testing::init_no_event_loop();
        let dir = std::env::temp_dir().join(format!("hestia-theme-manager-{}", std::process::id()));
        let store = Store::open(&dir).unwrap();
        let harbor = read_theme_pack(include_bytes!("../../../tools/theme-packs/harbor.zip")).unwrap();
        let blue = read_theme_pack(include_bytes!("../../../tools/theme-packs/blue.zip")).unwrap();
        store
            .save_appearance(&Appearance {
                theme: blue.base,
                active_pack: Some(blue.id.clone()),
                packs: vec![harbor, blue],
                launch_style: LaunchStyle::Orbit,
            })
            .unwrap();

        let ui = AppWindow::new().unwrap();
        install(&ui, dir.clone(), theme_file::Picker::new());
        ui.set_starting(false);
        ui.set_is_android(true);
        ui.show().unwrap();
        ui.window().set_size(slint::LogicalSize::new(432.0, 880.0));
        let settle = || {
            i_slint_backend_testing::mock_elapsed_time(Duration::from_millis(400));
            slint::platform::update_timers_and_animations();
        };
        let tap = |label: &str| {
            ElementHandle::find_by_accessible_label(&ui, label)
                .next()
                .unwrap_or_else(|| panic!("missing {label}"))
                .mock_single_click(PointerEventButton::Left);
            settle();
        };
        let key = |key: Key| {
            ui.window().dispatch_event(WindowEvent::KeyPressed { text: key.into() });
            ui.window()
                .dispatch_event(WindowEvent::KeyReleased { text: key.into() });
            settle();
        };

        ui.set_screen(3);
        settle();
        tap("Theme");
        tap("Manage theme packs");
        tap("Harbor");
        tap("Cancel");
        assert_eq!(store.load_appearance().unwrap().packs.len(), 2);

        tap("Theme");
        tap("Manage theme packs");
        assert!(ui.get_installed_theme_packs().iter().all(|pack| !pack.checked));
        key(Key::DownArrow);
        key(Key::Return);
        key(Key::DownArrow);
        key(Key::Return);
        assert!(ui.get_installed_theme_packs().iter().all(|pack| pack.checked));
        key(Key::DownArrow);
        key(Key::RightArrow);
        key(Key::Return);
        let saved = Store::open(&dir).unwrap().load_appearance().unwrap();
        assert!(saved.packs.is_empty());
        assert_eq!(saved.active_pack, None);
        assert_eq!(saved.theme, AppearanceTheme::Blue);
        assert_eq!(saved.launch_style, LaunchStyle::Orbit);
        assert_eq!(ui.get_saved_theme(), AppearanceTheme::Blue.index() as i32);
        assert!(ui.get_theme_menu().iter().all(|name| name != "Manage theme packs"));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn saving_a_pack_keeps_it_for_other_selections_and_replaces_the_same_id() {
        let dir = std::env::temp_dir().join(format!("hestia-theme-catalogue-{}", std::process::id()));
        let store = Store::open(&dir).unwrap();
        let starter = read_theme_pack(include_bytes!("../../../tools/theme-packs/harbor.zip")).unwrap();
        let imported = AppearanceTheme::ALL.len();
        let mut catalogue = Catalogue {
            preview: Some(starter.clone()),
            ..Default::default()
        };
        assert_eq!(
            catalogue.save(imported, LaunchStyle::Orbit, &store).unwrap(),
            imported as i32
        );
        let saved = store.load_appearance().unwrap();
        assert_eq!(saved.packs.len(), 1);
        assert_eq!(saved.active_pack.as_deref(), Some(starter.id.as_str()));
        assert_eq!(saved.launch_style, LaunchStyle::Orbit);
        assert_eq!(catalogue.save(1, LaunchStyle::Still, &store).unwrap(), 1);
        assert_eq!(store.load_appearance().unwrap().packs.len(), 1);

        let mut revision = starter;
        revision.name = "Harbor revision".into();
        catalogue.preview = Some(revision);
        assert_eq!(
            catalogue.save(imported + 1, LaunchStyle::Flame, &store).unwrap(),
            imported as i32
        );
        let saved = Store::open(&dir).unwrap().load_appearance().unwrap();
        assert_eq!(saved.packs.len(), 1);
        assert_eq!(saved.packs[0].name, "Harbor revision");
        assert_eq!(saved.launch_style, LaunchStyle::Flame);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
