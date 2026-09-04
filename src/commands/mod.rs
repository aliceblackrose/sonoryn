mod diagnostics;
mod play;
mod playback;
mod playback_controls;
mod playlist;
mod queue;
mod voice_control;

use crate::state::AppState;

pub(crate) fn command_list() -> Vec<gloam_commands::SlashCommand<AppState>> {
    let mut commands = diagnostics::command_list();
    commands.extend(play::command_list());
    commands.extend(
        playback::command_list()
            .into_iter()
            .filter(|command| command.descriptor().name != "play"),
    );
    commands.extend(playback_controls::command_list());
    commands.extend(playlist::command_list());
    commands.extend(queue::command_list());
    commands.extend(voice_control::command_list());
    commands
}
