use super::*;
use crate::{anyhow_expr, bail, bail_expr};

pub(crate) fn parse_macos_window(
    ac_params: &[SExpr],
    s: &ParserState,
) -> Result<&'static KanataAction> {
    let Some(first) = ac_params.first() else {
        bail!("macos-window expects a command name or at least one frame: (x y width height)");
    };

    if first.list(s.vars()).is_some() {
        return parse_frame_cycle(ac_params, s);
    }

    if ac_params.len() != 1 {
        bail!(
            "macos-window command form expects exactly one command name, found {} items",
            ac_params.len()
        );
    }

    let command = first
        .atom(s.vars())
        .ok_or_else(|| anyhow_expr!(first, "macos-window command must be a string"))?;
    let command = command.trim_atom_quotes();
    let Some(command) = parse_command(command) else {
        bail_expr!(first, "unknown macos-window command: {command}");
    };

    custom(
        CustomAction::MacosWindow(MacosWindowAction::Command(command)),
        &s.a,
    )
}

fn parse_frame_cycle(ac_params: &[SExpr], s: &ParserState) -> Result<&'static KanataAction> {
    let mut frames = Vec::with_capacity(ac_params.len());
    for frame in ac_params {
        let Some(values) = frame.list(s.vars()) else {
            bail_expr!(
                frame,
                "macos-window frame cycles must contain only lists: (x y width height)"
            );
        };
        if values.len() != 4 {
            bail_expr!(frame, "macos-window frames must have exactly four numbers");
        }
        let parsed = MacosWindowFrame {
            x: parse_basis_points(&values[0], s, "x")?,
            y: parse_basis_points(&values[1], s, "y")?,
            width: parse_basis_points(&values[2], s, "width")?,
            height: parse_basis_points(&values[3], s, "height")?,
        };
        if parsed.width <= 0 || parsed.height <= 0 {
            bail_expr!(
                frame,
                "macos-window width and height must be greater than 0"
            );
        }
        frames.push(parsed);
    }

    custom(
        CustomAction::MacosWindow(MacosWindowAction::FrameCycle(s.a.sref_vec(frames))),
        &s.a,
    )
}

fn parse_basis_points(expr: &SExpr, s: &ParserState, label: &str) -> Result<i32> {
    let Some(atom) = expr.atom(s.vars()) else {
        bail_expr!(expr, "macos-window {label} must be a number");
    };
    let atom = atom.trim_atom_quotes();
    Ok(atom
        .parse::<i32>()
        .map_err(|_| anyhow!("macos-window {label} must be a signed integer basis-point value"))?)
}

fn parse_command(command: &str) -> Option<MacosWindowCommand> {
    Some(match command {
        "maximize" => MacosWindowCommand::Maximize,
        "restore" => MacosWindowCommand::Restore,
        "maximize-height" => MacosWindowCommand::MaximizeHeight,
        "maximize-width" => MacosWindowCommand::MaximizeWidth,
        "almost-maximize" => MacosWindowCommand::AlmostMaximize,
        "reasonable-size" => MacosWindowCommand::ReasonableSize,
        "center" => MacosWindowCommand::Center,
        "center-half" => MacosWindowCommand::CenterHalf,
        "left-half" => MacosWindowCommand::LeftHalf,
        "right-half" => MacosWindowCommand::RightHalf,
        "top-half" => MacosWindowCommand::TopHalf,
        "bottom-half" => MacosWindowCommand::BottomHalf,
        "first-third" => MacosWindowCommand::FirstThird,
        "center-third" => MacosWindowCommand::CenterThird,
        "last-third" => MacosWindowCommand::LastThird,
        "first-two-thirds" => MacosWindowCommand::FirstTwoThirds,
        "center-two-thirds" => MacosWindowCommand::CenterTwoThirds,
        "last-two-thirds" => MacosWindowCommand::LastTwoThirds,
        "first-three-fourths" => MacosWindowCommand::FirstThreeFourths,
        "center-three-fourths" => MacosWindowCommand::CenterThreeFourths,
        "last-three-fourths" => MacosWindowCommand::LastThreeFourths,
        "top-third" => MacosWindowCommand::TopThird,
        "middle-third" => MacosWindowCommand::MiddleThird,
        "bottom-third" => MacosWindowCommand::BottomThird,
        "top-two-thirds" => MacosWindowCommand::TopTwoThirds,
        "bottom-two-thirds" => MacosWindowCommand::BottomTwoThirds,
        "top-first-fourth" => MacosWindowCommand::TopFirstFourth,
        "top-second-fourth" => MacosWindowCommand::TopSecondFourth,
        "top-third-fourth" => MacosWindowCommand::TopThirdFourth,
        "top-last-fourth" => MacosWindowCommand::TopLastFourth,
        "top-three-fourths" => MacosWindowCommand::TopThreeFourths,
        "bottom-three-fourths" => MacosWindowCommand::BottomThreeFourths,
        "top-center-two-thirds" => MacosWindowCommand::TopCenterTwoThirds,
        "bottom-center-two-thirds" => MacosWindowCommand::BottomCenterTwoThirds,
        "first-fourth" => MacosWindowCommand::FirstFourth,
        "second-fourth" => MacosWindowCommand::SecondFourth,
        "third-fourth" => MacosWindowCommand::ThirdFourth,
        "last-fourth" => MacosWindowCommand::LastFourth,
        "top-left-sixth" => MacosWindowCommand::TopLeftSixth,
        "top-center-sixth" => MacosWindowCommand::TopCenterSixth,
        "top-right-sixth" => MacosWindowCommand::TopRightSixth,
        "bottom-left-sixth" => MacosWindowCommand::BottomLeftSixth,
        "bottom-center-sixth" => MacosWindowCommand::BottomCenterSixth,
        "bottom-right-sixth" => MacosWindowCommand::BottomRightSixth,
        "move-left" => MacosWindowCommand::MoveLeft,
        "move-right" => MacosWindowCommand::MoveRight,
        "move-top" => MacosWindowCommand::MoveTop,
        "move-bottom" => MacosWindowCommand::MoveBottom,
        "move-to-previous-space" => MacosWindowCommand::MovePreviousSpace,
        "move-to-next-space" => MacosWindowCommand::MoveNextSpace,
        "switch-to-previous-space" | "switch-to-left-space" => {
            MacosWindowCommand::SwitchPreviousSpace
        }
        "switch-to-next-space" | "switch-to-right-space" => MacosWindowCommand::SwitchNextSpace,
        "move-to-previous-display" => MacosWindowCommand::MovePreviousDisplay,
        "move-to-next-display" => MacosWindowCommand::MoveNextDisplay,
        "top-left-quarter" => MacosWindowCommand::TopLeftQuarter,
        "top-right-quarter" => MacosWindowCommand::TopRightQuarter,
        "bottom-left-quarter" => MacosWindowCommand::BottomLeftQuarter,
        "bottom-right-quarter" => MacosWindowCommand::BottomRightQuarter,
        "make-smaller" => MacosWindowCommand::MakeSmaller,
        "make-larger" => MacosWindowCommand::MakeLarger,
        "toggle-fullscreen" => MacosWindowCommand::ToggleFullscreen,
        _ => return None,
    })
}
