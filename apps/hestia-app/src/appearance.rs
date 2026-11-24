//! Maps saved appearance sources to the current UI's theme catalogue.

use std::path::PathBuf;

use hestia_core::store::{Appearance, Store};
use slint::{ModelRc, SharedString, VecModel};

use crate::{AppWindow, AppearanceChoices};

// The renderer uses the same order for its built-in palettes and corner styles.
const BUILT_INS: [(&str, &str); 3] = [
    ("hearth", "Hearth (dark)"),
    ("daylight", "Daylight (light)"),
    ("midnight", "Midnight (dark)"),
];
const ICON_STYLES: [(&str, &str); 2] = [("monochrome", "Monochrome"), ("colorful", "Colorful")];

fn index(id: &str) -> Option<i32> {
    BUILT_INS.iter().position(|&(entry, _)| entry == id).map(|i| i as i32)
}

fn source(index: i32) -> Option<String> {
    BUILT_INS.get(index as usize).map(|&(id, _)| id.into())
}

impl From<Appearance> for AppearanceChoices {
    fn from(appearance: Appearance) -> Self {
        let override_index = |id: Option<String>| id.as_deref().and_then(index).map_or(0, |i| i + 1);
        Self {
            base: index(&appearance.base).unwrap_or(0),
            palette: override_index(appearance.palette),
            corners: override_index(appearance.corners),
            icons: ICON_STYLES
                .iter()
                .position(|&(id, _)| appearance.icons.as_deref() == Some(id))
                .map_or(0, |i| i as i32 + 1),
        }
    }
}

pub(crate) fn configure(ui: &AppWindow, data_dir: PathBuf) {
    let names = BUILT_INS.map(|(_, name)| SharedString::from(name));
    ui.set_theme_names(ModelRc::new(VecModel::from(names.to_vec())));
    let parts: Vec<_> = std::iter::once(SharedString::from("From base theme"))
        .chain(names)
        .collect();
    ui.set_theme_part_names(ModelRc::new(VecModel::from(parts)));
    let icon_names: Vec<_> = std::iter::once(SharedString::from("From base theme"))
        .chain(ICON_STYLES.map(|(_, label)| SharedString::from(label)))
        .collect();
    ui.set_icon_style_names(ModelRc::new(VecModel::from(icon_names)));

    match Store::open(&data_dir).and_then(|store| store.load_appearance()) {
        Ok(appearance) => ui.set_saved_appearance(appearance.into()),
        Err(e) => log::warn!("could not load appearance: {e}"),
    }
    ui.on_save_appearance(move |choices| {
        let Some(base) = source(choices.base) else {
            return false;
        };
        let appearance = Appearance {
            base,
            palette: source(choices.palette.saturating_sub(1)),
            corners: source(choices.corners.saturating_sub(1)),
            icons: ICON_STYLES
                .get(choices.icons.saturating_sub(1) as usize)
                .map(|&(id, _)| id.into()),
        };
        match Store::open(&data_dir).and_then(|store| store.save_appearance(&appearance)) {
            Ok(()) => true,
            Err(e) => {
                log::warn!("could not save appearance: {e}");
                false
            }
        }
    });
}
