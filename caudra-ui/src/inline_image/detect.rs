//! Picks how pictures are drawn. Kitty placeholders need a terminal that
//! answers the graphics query, and a multiplexer that lets the escape through;
//! anything else gets quadrant blocks, which every truecolor terminal draws.
//! Sextants are sharper but need a font that has them, so only a setting
//! asks for them.

use std::io;
#[cfg(unix)]
use std::time::Duration;

use caudra_config::InlineImages;
use caudra_terminal_host::terminal_detect::{
    inside_herdr, inside_tmux, tmux_passes_kitty_graphics,
};

use super::ImageMode;

#[cfg(unix)]
const KITTY_QUERY_TIMEOUT: Duration = Duration::from_millis(200);

/// What the surrounding terminal stack says about Kitty graphics.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Surroundings {
    pub herdr: bool,
    pub tmux: bool,
    /// tmux forwards the escapes and its client terminal draws placeholders.
    pub tmux_passes_kitty: bool,
}

impl Surroundings {
    fn current() -> Self {
        let tmux = inside_tmux();
        Self {
            herdr: inside_herdr(),
            tmux,
            tmux_passes_kitty: tmux && tmux_passes_kitty_graphics(),
        }
    }
}

/// Resolves the configured setting, asking the terminal only when nothing
/// else decides. Herdr stays on blocks unless Kitty is asked for by
/// name, because its placeholders have been seen drawing blank. Inside tmux
/// the query would reach tmux rather than the terminal, so its passthrough
/// setting and client terminal answer instead.
pub(crate) fn resolve(
    setting: InlineImages,
    around: Surroundings,
    probe: impl FnOnce() -> io::Result<bool>,
) -> ImageMode {
    match setting {
        InlineImages::Off => ImageMode::Off,
        InlineImages::Blocks => ImageMode::Blocks,
        InlineImages::Sextants => ImageMode::Sextants,
        InlineImages::Kitty => ImageMode::Kitty,
        InlineImages::Auto if around.herdr => ImageMode::Blocks,
        InlineImages::Auto if around.tmux => match around.tmux_passes_kitty {
            true => ImageMode::Kitty,
            false => ImageMode::Blocks,
        },
        InlineImages::Auto => match probe() {
            Ok(true) => ImageMode::Kitty,
            Ok(false) => ImageMode::Blocks,
            Err(error) => {
                tracing::debug!(%error, "kitty graphics query failed");
                ImageMode::Blocks
            }
        },
    }
}

/// Settles the mode for this terminal. Has to run in raw mode and before the
/// input reader starts, since the query reads the reply off the terminal.
pub(crate) fn detect(setting: InlineImages) -> ImageMode {
    let mode = resolve(setting, Surroundings::current(), query_kitty);
    tracing::info!(?setting, ?mode, "inline image mode");
    mode
}

fn query_kitty() -> io::Result<bool> {
    #[cfg(unix)]
    {
        crossterm::event::query_kitty_graphics(KITTY_QUERY_TIMEOUT)
    }
    #[cfg(not(unix))]
    {
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::{ImageMode, InlineImages, Surroundings, resolve};
    use test_case::test_case;

    const PLAIN: Surroundings = Surroundings {
        herdr: false,
        tmux: false,
        tmux_passes_kitty: false,
    };
    const HERDR: Surroundings = Surroundings {
        herdr: true,
        ..PLAIN
    };
    const TMUX_CLOSED: Surroundings = Surroundings {
        tmux: true,
        ..PLAIN
    };
    const TMUX_PASSING: Surroundings = Surroundings {
        tmux: true,
        tmux_passes_kitty: true,
        ..PLAIN
    };
    const PROBE_FAILED: &str = "no reply";

    #[derive(Clone, Copy, Debug)]
    enum Reply {
        Ok,
        Refused,
        Silent,
    }

    #[test_case(InlineImages::Auto, PLAIN, Reply::Ok => ImageMode::Kitty ; "auto_terminal_answers_ok")]
    #[test_case(InlineImages::Auto, PLAIN, Reply::Refused => ImageMode::Blocks ; "auto_terminal_refuses")]
    #[test_case(InlineImages::Auto, PLAIN, Reply::Silent => ImageMode::Blocks ; "auto_terminal_silent")]
    #[test_case(InlineImages::Auto, HERDR, Reply::Ok => ImageMode::Blocks ; "auto_herdr_stays_blocks")]
    #[test_case(InlineImages::Kitty, HERDR, Reply::Refused => ImageMode::Kitty ; "kitty_by_name_inside_herdr")]
    #[test_case(InlineImages::Auto, TMUX_CLOSED, Reply::Ok => ImageMode::Blocks ; "auto_tmux_without_passthrough")]
    #[test_case(InlineImages::Auto, TMUX_PASSING, Reply::Refused => ImageMode::Kitty ; "auto_tmux_with_passthrough")]
    #[test_case(InlineImages::Blocks, PLAIN, Reply::Ok => ImageMode::Blocks ; "blocks_by_name")]
    #[test_case(InlineImages::Sextants, HERDR, Reply::Ok => ImageMode::Sextants ; "sextants_by_name")]
    #[test_case(InlineImages::Off, PLAIN, Reply::Ok => ImageMode::Off ; "off")]
    fn the_setting_and_terminal_pick_the_mode(
        setting: InlineImages,
        around: Surroundings,
        reply: Reply,
    ) -> ImageMode {
        resolve(setting, around, || match reply {
            Reply::Ok => Ok(true),
            Reply::Refused => Ok(false),
            Reply::Silent => Err(io::Error::other(PROBE_FAILED)),
        })
    }

    #[test_case(HERDR ; "herdr")]
    #[test_case(TMUX_CLOSED ; "tmux")]
    #[test_case(TMUX_PASSING ; "tmux_passing")]
    fn a_multiplexer_is_never_probed(around: Surroundings) {
        resolve(InlineImages::Auto, around, || unreachable!("probed"));
    }
}
