mod diagnostics;
mod music;
mod voice_control;

use crate::state::AppState;

pub(crate) fn command_list() -> Vec<gloam_commands::SlashCommand<AppState>> {
    let mut commands = diagnostics::command_list();
    commands.extend(music::command_list());
    commands.extend(voice_control::command_list());
    commands
}
