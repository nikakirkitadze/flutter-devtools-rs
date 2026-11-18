//! The fdt application: shell, navigation, palette and settings.
pub mod actions;
pub mod app;
pub mod keymap;
pub mod logging;
pub mod menus;
pub mod nav;
pub mod palette;
pub mod settings;
pub mod shell;
pub mod startup;
#[cfg(test)]
pub(crate) mod test_support;
