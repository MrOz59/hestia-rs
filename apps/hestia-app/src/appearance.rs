use std::collections::HashSet;
use std::fmt::Write;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use hestia_core::appearance::{Appearance, LaunchStyle, PaletteRole, ThemePack, read_theme_pack};
use hestia_core::store::{AppearanceTheme, Store};
use slint::{Color, ComponentHandle, Model, ModelRc, SharedString, StyledText, VecModel};

use crate::{AppWindow, InstalledThemePack, PaletteOverride, Theme, ThemeStyle, VoltagePaths, theme_file};

const APPEARANCE_ERROR: i32 = -2;

#[derive(Default)]
struct Catalogue {
    saved: Appearance,
    preview: Option<ThemePack>,
    reading_file: bool,
}

#[derive(Clone, Copy)]
enum Selection<'a> {
    BuiltIn(AppearanceTheme),
    Imported(&'a ThemePack),
    Preview(&'a ThemePack),
}

impl Selection<'_> {
    fn base(self) -> AppearanceTheme {
        match self {
            Self::BuiltIn(base) => base,
            Self::Imported(pack) | Self::Preview(pack) => pack.base,
        }
    }
}

impl Catalogue {
    fn choices(&self) -> impl Iterator<Item = Selection<'_>> {
        AppearanceTheme::ALL
            .into_iter()
            .filter(|theme| !self.saved.hidden_themes.contains(theme))
            .map(Selection::BuiltIn)
            .chain(self.saved.packs.iter().map(Selection::Imported))
            .chain(self.preview.iter().map(Selection::Preview))
    }

    fn index(&self, selection: Selection<'_>) -> Option<i32> {
        self.choices()
            .position(|entry| match (selection, entry) {
                (Selection::BuiltIn(a), Selection::BuiltIn(b)) => a == b,
                (Selection::Imported(a), Selection::Imported(b)) | (Selection::Preview(a), Selection::Preview(b)) => {
                    a.id == b.id
                }
                _ => false,
            })
            .map(|i| i as i32)
    }

    fn active_index(&self) -> i32 {
        self.saved
            .active_pack
            .as_ref()
            .and_then(|id| self.saved.packs.iter().find(|pack| &pack.id == id))
            .and_then(|pack| self.index(Selection::Imported(pack)))
            .or_else(|| self.index(Selection::BuiltIn(self.saved.theme)))
            .unwrap_or(-1)
    }

    fn ensure_active(&mut self) {
        if self
            .saved
            .active_pack
            .as_ref()
            .is_some_and(|id| self.saved.packs.iter().any(|pack| &pack.id == id))
        {
            return;
        }
        self.saved.active_pack = None;
        if !self.saved.hidden_themes.contains(&self.saved.theme) {
            return;
        }
        if let Some(theme) = AppearanceTheme::ALL
            .into_iter()
            .find(|theme| !self.saved.hidden_themes.contains(theme))
        {
            self.saved.theme = theme;
        } else if let Some(pack) = self.saved.packs.first() {
            self.saved.theme = pack.base;
            self.saved.active_pack = Some(pack.id.clone());
        } else {
            self.saved.theme = AppearanceTheme::Hearth;
        }
    }

    fn update_options(&self, ui: &AppWindow) {
        let builtin_names = ui.get_builtin_theme_names();
        let mut names: Vec<SharedString> = Vec::new();
        let mut styles = Vec::new();
        for choice in self.choices() {
            let mut style = ThemeStyle {
                base: choice.base().index() as i32,
                ..Default::default()
            };
            match choice {
                Selection::BuiltIn(base) => names.push(builtin_names.row_data(base.index()).unwrap_or_default()),
                Selection::Imported(pack) | Selection::Preview(pack) => {
                    let update = matches!(choice, Selection::Preview(_))
                        && self.saved.packs.iter().any(|saved| saved.id == pack.id);
                    names.push(if update {
                        format!("{} (preview)", pack.name).into()
                    } else {
                        pack.name.as_str().into()
                    });
                    style.colors = model(
                        PaletteRole::ALL
                            .into_iter()
                            .map(|role| {
                                pack.color(role)
                                    .map_or_else(PaletteOverride::default, |[red, green, blue, alpha]| {
                                        PaletteOverride {
                                            supplied: true,
                                            color: Color::from_argb_u8(alpha, red, green, blue),
                                        }
                                    })
                            })
                            .collect(),
                    );
                }
            }
            styles.push(style);
        }
        let mut menu = names.clone();
        menu.push("Manage themes".into());
        menu.push("Load Theme Pack from File".into());
        ui.set_theme_names(model(names));
        ui.set_theme_menu(model(menu));
        ui.set_theme_styles(model(styles));
        ui.set_can_restore_themes(!self.saved.hidden_themes.is_empty());
    }

    fn open_manager(&self, ui: &AppWindow) {
        let active = self.active_index();
        let builtin_names = ui.get_builtin_theme_names();
        let rows = self
            .choices()
            .enumerate()
            .filter_map(|(i, choice)| {
                let active = i as i32 == active;
                match choice {
                    Selection::BuiltIn(theme) => Some(InstalledThemePack {
                        name: builtin_names.row_data(theme.index()).unwrap_or_default(),
                        builtin_index: theme.index() as i32,
                        active,
                        ..Default::default()
                    }),
                    Selection::Imported(pack) => Some(InstalledThemePack {
                        id: pack.id.as_str().into(),
                        name: pack.name.as_str().into(),
                        builtin_index: -1,
                        active,
                        ..Default::default()
                    }),
                    Selection::Preview(_) => None,
                }
            })
            .collect();
        ui.set_installed_theme_packs(model(rows));
    }

    fn remove(
        &mut self,
        builtins: &[AppearanceTheme],
        ids: &HashSet<&str>,
        draft: i32,
        store: &Store,
    ) -> std::io::Result<i32> {
        let mut next = self.snapshot();
        next.saved.hidden_themes.extend_from_slice(builtins);
        next.saved.packs.retain(|pack| !ids.contains(pack.id.as_str()));
        if next.preview.as_ref().is_some_and(|pack| ids.contains(pack.id.as_str())) {
            next.preview = None;
        }
        self.apply_catalogue(next, draft, store)
    }

    fn restore(&mut self, draft: i32, store: &Store) -> std::io::Result<i32> {
        let mut next = self.snapshot();
        next.saved.hidden_themes.clear();
        self.apply_catalogue(next, draft, store)
    }

    fn snapshot(&self) -> Self {
        Self {
            saved: self.saved.clone(),
            preview: self.preview.clone(),
            reading_file: self.reading_file,
        }
    }

    fn apply_catalogue(&mut self, mut next: Self, draft: i32, store: &Store) -> std::io::Result<i32> {
        next.ensure_active();
        let index = usize::try_from(draft)
            .ok()
            .and_then(|i| self.choices().nth(i))
            .and_then(|selection| match selection {
                Selection::BuiltIn(_) => next.index(selection),
                Selection::Imported(_) | Selection::Preview(_) => next
                    .index(selection)
                    .or_else(|| next.index(Selection::BuiltIn(selection.base()))),
            })
            .unwrap_or_else(|| next.active_index());
        store.save_appearance(&next.saved)?;
        *self = next;
        Ok(index)
    }

    fn save(&mut self, index: i32, style: LaunchStyle, store: &Store) -> std::io::Result<i32> {
        let mut saved = self.saved.clone();
        saved.launch_style = style;
        match usize::try_from(index).ok().and_then(|i| self.choices().nth(i)) {
            Some(Selection::BuiltIn(base)) => {
                saved.theme = base;
                saved.active_pack = None;
            }
            Some(Selection::Imported(pack) | Selection::Preview(pack)) => {
                saved.theme = pack.base;
                saved.active_pack = Some(pack.id.clone());
                if let Some(existing) = saved.packs.iter_mut().find(|existing| existing.id == pack.id) {
                    *existing = pack.clone();
                } else {
                    saved.packs.push(pack.clone());
                }
            }
            None if index == -1 && self.active_index() == -1 => {}
            None => return Err(std::io::Error::other("theme selection no longer exists")),
        }
        store.save_appearance(&saved)?;
        self.saved = saved;
        self.preview = None;
        Ok(self.active_index())
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
    state.ensure_active();
    state.update_options(ui);
    ui.set_saved_theme(state.active_index());
    ui.set_saved_launch_style(state.saved.launch_style.index() as i32);
    let state = Arc::new(Mutex::new(state));

    let window = ui.as_weak();
    let catalogue = state.clone();
    let save_dir = data_dir.clone();
    ui.on_save_theme(move |index, launch| {
        let Some(&style) = LaunchStyle::ALL.get(launch as usize) else {
            return APPEARANCE_ERROR;
        };
        let Some(window) = window.upgrade() else {
            return APPEARANCE_ERROR;
        };
        let mut catalogue = catalogue.lock().unwrap();
        match Store::open(&save_dir).and_then(|store| catalogue.save(index, style, &store)) {
            Ok(index) => {
                catalogue.update_options(&window);
                index
            }
            Err(error) => {
                log::warn!("could not save appearance: {error}");
                APPEARANCE_ERROR
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
    let remove_dir = data_dir.clone();
    ui.on_remove_theme_packs(move |draft| {
        let Some(window) = window.upgrade() else {
            return APPEARANCE_ERROR;
        };
        let selected: Vec<_> = window
            .get_installed_theme_packs()
            .iter()
            .filter(|pack| pack.checked)
            .collect();
        let builtins = selected
            .iter()
            .filter_map(|row| AppearanceTheme::ALL.get(row.builtin_index as usize).copied())
            .collect::<Vec<_>>();
        let ids = selected
            .iter()
            .filter(|row| row.builtin_index < 0)
            .map(|row| row.id.as_str())
            .collect();
        let mut catalogue = catalogue.lock().unwrap();
        let had_preview = catalogue.preview.is_some();
        match Store::open(&remove_dir).and_then(|store| catalogue.remove(&builtins, &ids, draft, &store)) {
            Ok(index) => {
                catalogue.update_options(&window);
                window.set_saved_theme(catalogue.active_index());
                if had_preview && catalogue.preview.is_none() {
                    window.set_theme_import_message("".into());
                }
                index
            }
            Err(error) => {
                log::warn!("could not remove themes: {error}");
                APPEARANCE_ERROR
            }
        }
    });

    let window = ui.as_weak();
    let catalogue = state.clone();
    ui.on_restore_builtin_themes(move |draft| {
        let Some(window) = window.upgrade() else {
            return APPEARANCE_ERROR;
        };
        let mut catalogue = catalogue.lock().unwrap();
        match Store::open(&data_dir).and_then(|store| catalogue.restore(draft, &store)) {
            Ok(index) => {
                catalogue.update_options(&window);
                window.set_saved_theme(catalogue.active_index());
                index
            }
            Err(error) => {
                log::warn!("could not restore built-in themes: {error}");
                APPEARANCE_ERROR
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
                        if let Some(index) = catalogue
                            .preview
                            .as_ref()
                            .and_then(|pack| catalogue.index(Selection::Preview(pack)))
                        {
                            ui.invoke_preview_theme(index);
                        }
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
                ..Default::default()
            },
            preview: Some(harbor.clone()),
            ..Default::default()
        };
        store.save_appearance(&catalogue.saved).unwrap();
        let first_pack = AppearanceTheme::ALL.len() as i32;
        let selected = HashSet::from([harbor.id.as_str()]);
        let obstruction = dir.join(".appearance.json.tmp");
        std::fs::create_dir(&obstruction).unwrap();
        assert!(catalogue.remove(&[], &selected, first_pack + 1, &store).is_err());
        assert_eq!(catalogue.active_index(), first_pack + 1);
        assert!(catalogue.preview.is_some());
        std::fs::remove_dir(obstruction).unwrap();

        assert_eq!(
            catalogue.remove(&[], &selected, first_pack + 1, &store).unwrap(),
            first_pack
        );
        assert_eq!(catalogue.active_index(), first_pack);
        assert!(catalogue.preview.is_none());
        let saved = Store::open(&dir).unwrap().load_appearance().unwrap();
        assert_eq!(saved.active_pack.as_deref(), Some(blue.id.as_str()));
        assert_eq!(saved.packs.len(), 1);
        assert_eq!(saved.launch_style, LaunchStyle::Orbit);

        // The imported pack stays eligible even when its built-in base is hidden.
        assert_eq!(
            catalogue
                .remove(&AppearanceTheme::ALL, &HashSet::new(), first_pack, &store)
                .unwrap(),
            0
        );
        assert_eq!(catalogue.active_index(), 0);
        assert_eq!(catalogue.restore(0, &store).unwrap(), first_pack);
        assert_eq!(catalogue.active_index(), first_pack);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(all(target_os = "linux", debug_assertions))]
    #[test]
    fn manager_removes_all_themes_with_a_remote_and_restores_builtins() {
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
                ..Default::default()
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
        tap("Manage themes");
        assert_eq!(
            ui.get_installed_theme_packs().row_count(),
            AppearanceTheme::ALL.len() + 2
        );
        tap("Harbor");
        tap("Cancel");
        assert_eq!(store.load_appearance().unwrap().packs.len(), 2);

        tap("Theme");
        tap("Manage themes");
        assert!(ui.get_installed_theme_packs().iter().all(|pack| !pack.checked));
        key(Key::DownArrow);
        for i in 0..ui.get_installed_theme_packs().row_count() {
            if i > 0 {
                key(Key::DownArrow);
            }
            key(Key::Return);
        }
        assert!(ui.get_installed_theme_packs().iter().all(|pack| pack.checked));
        key(Key::DownArrow);
        key(Key::RightArrow);
        key(Key::Return);
        let saved = Store::open(&dir).unwrap().load_appearance().unwrap();
        assert!(saved.packs.is_empty());
        assert_eq!(saved.active_pack, None);
        assert_eq!(saved.hidden_themes.len(), AppearanceTheme::ALL.len());
        assert_eq!(saved.theme, AppearanceTheme::Hearth);
        assert_eq!(saved.launch_style, LaunchStyle::Orbit);
        assert_eq!(ui.get_saved_theme(), -1);
        assert_eq!(ui.get_theme_names().row_count(), 0);
        assert_eq!(ui.global::<Theme>().get_selection(), 0);

        tap("Theme");
        tap("Manage themes");
        assert_eq!(ui.get_installed_theme_packs().row_count(), 0);
        tap("Restore built-in themes");
        assert!(store.load_appearance().unwrap().hidden_themes.is_empty());
        assert_eq!(ui.get_theme_names().row_count(), AppearanceTheme::ALL.len());
        assert_eq!(ui.get_saved_theme(), 0);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn saving_a_pack_keeps_it_for_other_selections_and_replaces_the_same_id() {
        let dir = std::env::temp_dir().join(format!("hestia-theme-catalogue-{}", std::process::id()));
        let store = Store::open(&dir).unwrap();
        let starter = read_theme_pack(include_bytes!("../../../tools/theme-packs/harbor.zip")).unwrap();
        let imported = AppearanceTheme::ALL.len() as i32;
        let mut catalogue = Catalogue {
            preview: Some(starter.clone()),
            ..Default::default()
        };
        assert_eq!(catalogue.save(imported, LaunchStyle::Orbit, &store).unwrap(), imported);
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
            imported
        );
        let saved = Store::open(&dir).unwrap().load_appearance().unwrap();
        assert_eq!(saved.packs.len(), 1);
        assert_eq!(saved.packs[0].name, "Harbor revision");
        assert_eq!(saved.launch_style, LaunchStyle::Flame);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
