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
use crossterm::event::TerminalGraphics;

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

pub(crate) struct Detection {
    pub mode: ImageMode,
    pub cell_pixels: Option<(u16, u16)>,
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
/// else decides. Herdr still draws blank placeholders despite accepting PNG
/// queries, so automatic mode retains its character fallback.
/// Inside tmux the query would reach tmux rather than the terminal, so its passthrough
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
        InlineImages::Auto if around.herdr => {
            tracing::debug!(
                reason = "herdr_placeholders_unverified",
                "inline image fallback"
            );
            ImageMode::Blocks
        }
        InlineImages::Auto if around.tmux => match around.tmux_passes_kitty {
            true => ImageMode::Kitty,
            false => {
                tracing::debug!(
                    reason = "tmux_passthrough_or_placeholders_unavailable",
                    "inline image fallback"
                );
                ImageMode::Blocks
            }
        },
        InlineImages::Auto => match probe() {
            Ok(true) => ImageMode::Kitty,
            Ok(false) => {
                tracing::debug!(reason = "no_matching_png_reply", "inline image fallback");
                ImageMode::Blocks
            }
            Err(error) => {
                tracing::debug!(%error, "kitty graphics query failed");
                ImageMode::Blocks
            }
        },
    }
}

/// Settles the mode for this terminal. Has to run in raw mode and before the
/// input reader starts, since the query reads the reply off the terminal.
pub(crate) fn detect(setting: InlineImages) -> Detection {
    let around = Surroundings::current();
    let mut cell_pixels = None;
    let mode = resolve(setting, around, || {
        let reply = query_kitty()?;
        cell_pixels = reply.cell_pixels;
        Ok(reply.kitty)
    });
    tracing::info!(
        ?setting,
        ?mode,
        herdr = around.herdr,
        tmux = around.tmux,
        tmux_passthrough = around.tmux_passes_kitty,
        ?cell_pixels,
        "inline image mode"
    );
    Detection { mode, cell_pixels }
}

fn query_kitty() -> io::Result<TerminalGraphics> {
    #[cfg(unix)]
    {
        crossterm::event::query_terminal_graphics(KITTY_QUERY_TIMEOUT)
    }
    #[cfg(not(unix))]
    {
        Ok(TerminalGraphics::default())
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
    #[test_case(InlineImages::Auto, HERDR, Reply::Ok => ImageMode::Blocks ; "png_reply_does_not_prove_herdr_placement")]
    #[test_case(InlineImages::Auto, HERDR, Reply::Refused => ImageMode::Blocks ; "auto_herdr_disabled")]
    #[test_case(InlineImages::Auto, HERDR, Reply::Silent => ImageMode::Blocks ; "auto_herdr_unknown")]
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

    #[test_case(TMUX_CLOSED ; "tmux")]
    #[test_case(TMUX_PASSING ; "tmux_passing")]
    #[test_case(HERDR ; "herdr_unverified")]
    fn guarded_hosts_are_never_probed(around: Surroundings) {
        resolve(InlineImages::Auto, around, || unreachable!("probed"));
    }

    #[test_case(InlineImages::Off)]
    #[test_case(InlineImages::Blocks)]
    #[test_case(InlineImages::Sextants)]
    #[test_case(InlineImages::Kitty)]
    fn explicit_modes_never_probe(setting: InlineImages) {
        resolve(setting, HERDR, || unreachable!("probed"));
    }
}
