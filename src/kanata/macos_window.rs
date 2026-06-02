use anyhow::{Result, anyhow, bail};
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::display::{CGPoint, CGRect, CGSize};
use kanata_parser::custom_action::{MacosWindowAction, MacosWindowCommand, MacosWindowFrame};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

type AXUIElementRef = *const c_void;
type AXError = i32;
type CGDirectDisplayID = u32;
type CGError = i32;
type CGWindowID = u32;

const K_AX_VALUE_CGPOINT_TYPE: i32 = 1;
const K_AX_VALUE_CGSIZE_TYPE: i32 = 2;
const K_AX_ERROR_SUCCESS: AXError = 0;
const KCG_ERROR_SUCCESS: CGError = 0;
const K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY: u32 = 1;
const K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS: u32 = 16;
const K_CG_NULL_WINDOW_ID: CGWindowID = 0;
const BASIS_POINTS: f64 = 10_000.0;
const CYCLE_RESET_AFTER: Duration = Duration::from_millis(900);
const RESIZE_DELTA_PX: f64 = 32.0;
const MIN_WINDOW_EDGE_PX: f64 = 64.0;

const LEFT_HALF_FRAMES: [MacosWindowFrame; 3] = [
    bp(0, 0, 5000, 10000),
    bp(0, 0, 6667, 10000),
    bp(0, 0, 3333, 10000),
];
const RIGHT_HALF_FRAMES: [MacosWindowFrame; 3] = [
    bp(5000, 0, 5000, 10000),
    bp(3333, 0, 6667, 10000),
    bp(6667, 0, 3333, 10000),
];
const TOP_HALF_FRAMES: [MacosWindowFrame; 3] = [
    bp(0, 0, 10000, 5000),
    bp(0, 0, 10000, 6667),
    bp(0, 0, 10000, 3333),
];
const BOTTOM_HALF_FRAMES: [MacosWindowFrame; 3] = [
    bp(0, 5000, 10000, 5000),
    bp(0, 3333, 10000, 6667),
    bp(0, 6667, 10000, 3333),
];
const CENTER_HALF_FRAMES: [MacosWindowFrame; 3] = [
    bp(2500, 0, 5000, 10000),
    bp(1667, 0, 6667, 10000),
    bp(3333, 0, 3333, 10000),
];

const fn bp(x: i32, y: i32, width: i32, height: i32) -> MacosWindowFrame {
    MacosWindowFrame {
        x,
        y,
        width,
        height,
    }
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    fn AXUIElementCopyAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: *mut CFTypeRef,
    ) -> AXError;
    fn AXUIElementSetAttributeValue(
        element: AXUIElementRef,
        attribute: CFStringRef,
        value: CFTypeRef,
    ) -> AXError;
    fn AXValueCreate(value_type: i32, value: *const c_void) -> CFTypeRef;
    fn AXValueGetValue(value: CFTypeRef, value_type: i32, out: *mut c_void) -> bool;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGGetActiveDisplayList(
        max_displays: u32,
        active_displays: *mut CGDirectDisplayID,
        display_count: *mut u32,
    ) -> CGError;
    fn CGMainDisplayID() -> CGDirectDisplayID;
    fn CGDisplayBounds(display: CGDirectDisplayID) -> CGRect;
    fn CGWindowListCopyWindowInfo(option: u32, relative_to_window: CGWindowID) -> CFArrayRef;
    fn CGSMainConnectionID() -> i32;
    fn CGSCopyManagedDisplayForWindow(cid: i32, window: CGWindowID) -> CFStringRef;
    fn CGSCopyManagedDisplaySpaces(cid: i32) -> CFArrayRef;
    fn CGSManagedDisplaySetCurrentSpace(
        cid: i32,
        display_identifier: CFStringRef,
        space: u64,
    ) -> CGError;
    fn CGSShowSpaces(cid: i32, spaces: CFArrayRef) -> CGError;
    fn CGSMoveWindowsToManagedSpace(cid: i32, windows: CFArrayRef, space: u64) -> CGError;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CycleKey {
    frames_ptr: usize,
    frames_len: usize,
}

#[derive(Debug)]
struct CycleState {
    key: Option<CycleKey>,
    step: usize,
    last_used: Instant,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct WindowId(u32);

struct WindowContext {
    _app: CFType,
    _window: CFType,
    element: AXUIElementRef,
    id: WindowId,
    frame: CGRect,
    screen: CGRect,
}

static CYCLE_STATE: OnceLock<Mutex<CycleState>> = OnceLock::new();
static RESTORE_FRAMES: OnceLock<Mutex<HashMap<WindowId, CGRect>>> = OnceLock::new();

pub fn apply_window_action(action: &MacosWindowAction) -> Result<()> {
    let ctx = WindowContext::focused()?;
    match action {
        MacosWindowAction::FrameCycle(frames) => apply_frame_cycle(&ctx, frames),
        MacosWindowAction::Command(command) => apply_command(&ctx, *command),
    }
}

fn apply_command(ctx: &WindowContext, command: MacosWindowCommand) -> Result<()> {
    use MacosWindowCommand::*;

    match command {
        Restore => restore(ctx),
        ToggleFullscreen => toggle_fullscreen(ctx),
        LeftHalf => apply_frame_cycle(ctx, &LEFT_HALF_FRAMES),
        RightHalf => apply_frame_cycle(ctx, &RIGHT_HALF_FRAMES),
        TopHalf => apply_frame_cycle(ctx, &TOP_HALF_FRAMES),
        BottomHalf => apply_frame_cycle(ctx, &BOTTOM_HALF_FRAMES),
        CenterHalf => apply_frame_cycle(ctx, &CENTER_HALF_FRAMES),
        Maximize => set_window_frame_remembering(ctx, ctx.screen),
        MaximizeHeight => set_window_frame_remembering(
            ctx,
            rect(
                ctx.frame.origin.x,
                ctx.screen.origin.y,
                ctx.frame.size.width,
                ctx.screen.size.height,
            ),
        ),
        MaximizeWidth => set_window_frame_remembering(
            ctx,
            rect(
                ctx.screen.origin.x,
                ctx.frame.origin.y,
                ctx.screen.size.width,
                ctx.frame.size.height,
            ),
        ),
        AlmostMaximize => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(500, 500, 9000, 9000)))
        }
        ReasonableSize => set_window_frame_remembering(ctx, reasonable_size(ctx.screen)),
        Center => set_window_frame_remembering(ctx, center_current_size(ctx.screen, ctx.frame)),
        FirstThird => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(0, 0, 3333, 10000)))
        }
        CenterThird => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(3333, 0, 3334, 10000)))
        }
        LastThird => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(6667, 0, 3333, 10000)))
        }
        FirstTwoThirds => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(0, 0, 6667, 10000)))
        }
        CenterTwoThirds => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(1667, 0, 6666, 10000)))
        }
        LastTwoThirds => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(3333, 0, 6667, 10000)))
        }
        FirstThreeFourths => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(0, 0, 7500, 10000)))
        }
        CenterThreeFourths => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(1250, 0, 7500, 10000)))
        }
        LastThreeFourths => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(2500, 0, 7500, 10000)))
        }
        TopThird => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(0, 0, 10000, 3333)))
        }
        MiddleThird => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(0, 3333, 10000, 3334)))
        }
        BottomThird => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(0, 6667, 10000, 3333)))
        }
        TopTwoThirds => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(0, 0, 10000, 6667)))
        }
        BottomTwoThirds => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(0, 3333, 10000, 6667)))
        }
        TopFirstFourth => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(0, 0, 2500, 5000)))
        }
        TopSecondFourth => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(2500, 0, 2500, 5000)))
        }
        TopThirdFourth => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(5000, 0, 2500, 5000)))
        }
        TopLastFourth => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(7500, 0, 2500, 5000)))
        }
        TopThreeFourths => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(0, 0, 10000, 7500)))
        }
        BottomThreeFourths => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(0, 2500, 10000, 7500)))
        }
        TopCenterTwoThirds => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(1667, 0, 6666, 5000)))
        }
        BottomCenterTwoThirds => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(1667, 5000, 6666, 5000)))
        }
        FirstFourth => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(0, 0, 2500, 10000)))
        }
        SecondFourth => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(2500, 0, 2500, 10000)))
        }
        ThirdFourth => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(5000, 0, 2500, 10000)))
        }
        LastFourth => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(7500, 0, 2500, 10000)))
        }
        TopLeftSixth => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(0, 0, 3333, 5000)))
        }
        TopCenterSixth => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(3333, 0, 3334, 5000)))
        }
        TopRightSixth => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(6667, 0, 3333, 5000)))
        }
        BottomLeftSixth => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(0, 5000, 3333, 5000)))
        }
        BottomCenterSixth => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(3333, 5000, 3334, 5000)))
        }
        BottomRightSixth => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(6667, 5000, 3333, 5000)))
        }
        TopLeftQuarter => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(0, 0, 5000, 5000)))
        }
        TopRightQuarter => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(5000, 0, 5000, 5000)))
        }
        BottomLeftQuarter => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(0, 5000, 5000, 5000)))
        }
        BottomRightQuarter => {
            set_window_frame_remembering(ctx, resolve_frame(ctx.screen, bp(5000, 5000, 5000, 5000)))
        }
        MoveLeft => {
            set_window_frame_remembering(ctx, move_to_edge(ctx.frame, ctx.screen, MoveEdge::Left))
        }
        MoveRight => {
            set_window_frame_remembering(ctx, move_to_edge(ctx.frame, ctx.screen, MoveEdge::Right))
        }
        MoveTop => {
            set_window_frame_remembering(ctx, move_to_edge(ctx.frame, ctx.screen, MoveEdge::Top))
        }
        MoveBottom => {
            set_window_frame_remembering(ctx, move_to_edge(ctx.frame, ctx.screen, MoveEdge::Bottom))
        }
        MovePreviousSpace => move_window_to_space(ctx, SpaceStep::Previous),
        MoveNextSpace => move_window_to_space(ctx, SpaceStep::Next),
        SwitchPreviousSpace => switch_space(ctx, SpaceStep::Previous),
        SwitchNextSpace => switch_space(ctx, SpaceStep::Next),
        MovePreviousDisplay => move_to_display(ctx, DisplayStep::Previous),
        MoveNextDisplay => move_to_display(ctx, DisplayStep::Next),
        MakeSmaller => set_window_frame_remembering(
            ctx,
            resize_from_center(ctx.frame, ctx.screen, -RESIZE_DELTA_PX),
        ),
        MakeLarger => set_window_frame_remembering(
            ctx,
            resize_from_center(ctx.frame, ctx.screen, RESIZE_DELTA_PX),
        ),
    }
}

fn apply_frame_cycle(ctx: &WindowContext, frames: &'static [MacosWindowFrame]) -> Result<()> {
    if frames.is_empty() {
        bail!("macos-window action has no frames");
    }
    let key = CycleKey {
        frames_ptr: frames.as_ptr() as usize,
        frames_len: frames.len(),
    };
    let frame = resolve_frame(ctx.screen, frames[next_cycle_index(key)]);
    set_window_frame_remembering(ctx, frame)
}

fn next_cycle_index(key: CycleKey) -> usize {
    let now = Instant::now();
    let state = CYCLE_STATE.get_or_init(|| {
        Mutex::new(CycleState {
            key: None,
            step: 0,
            last_used: now,
        })
    });
    let mut state = state.lock();
    if state.key == Some(key) && now.duration_since(state.last_used) <= CYCLE_RESET_AFTER {
        state.step = (state.step + 1) % key.frames_len;
    } else {
        state.key = Some(key);
        state.step = 0;
    }
    state.last_used = now;
    state.step
}

impl WindowContext {
    fn focused() -> Result<Self> {
        let info = frontmost_window_info()?;
        let app = unsafe { AXUIElementCreateApplication(info.pid) };
        if app.is_null() {
            bail!("AXUIElementCreateApplication({}) returned null", info.pid);
        }
        let app = unsafe { CFType::wrap_under_create_rule(app as CFTypeRef) };
        let app_element = app.as_CFTypeRef() as AXUIElementRef;
        let window = copy_ax_attr(app_element, "AXFocusedWindow")
            .or_else(|_| copy_ax_attr(app_element, "AXMainWindow"))?;
        let element = window.as_CFTypeRef() as AXUIElementRef;
        let frame = window_frame(element)?;
        let screen = screen_for_frame(frame)?;
        Ok(Self {
            _app: app,
            _window: window,
            element,
            id: WindowId(info.window_id),
            frame,
            screen,
        })
    }
}

#[derive(Clone, Copy, Debug)]
struct FrontmostWindowInfo {
    pid: i32,
    window_id: u32,
}

fn frontmost_window_info() -> Result<FrontmostWindowInfo> {
    let options =
        K_CG_WINDOW_LIST_OPTION_ON_SCREEN_ONLY | K_CG_WINDOW_LIST_EXCLUDE_DESKTOP_ELEMENTS;
    let windows = unsafe { CGWindowListCopyWindowInfo(options, K_CG_NULL_WINDOW_ID) };
    if windows.is_null() {
        bail!("CGWindowListCopyWindowInfo returned null");
    }
    let windows = unsafe { CFArray::<CFType>::wrap_under_create_rule(windows) };
    for window in windows.iter() {
        let Some(window) = window.downcast::<CFDictionary>() else {
            continue;
        };
        let window = unsafe {
            CFDictionary::<CFString, CFType>::wrap_under_get_rule(window.as_concrete_TypeRef())
        };
        let layer = dict_i32(&window, "kCGWindowLayer");
        let pid = dict_i32(&window, "kCGWindowOwnerPID");
        let window_id = dict_i32(&window, "kCGWindowNumber");
        if layer == Some(0)
            && let (Some(pid), Some(window_id)) = (pid, window_id)
        {
            return Ok(FrontmostWindowInfo {
                pid,
                window_id: window_id as u32,
            });
        }
    }
    bail!("could not find a frontmost on-screen window")
}

fn dict_i32(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<i32> {
    let key = CFString::new(key);
    dict.find(&key)
        .and_then(|value| value.downcast::<CFNumber>())
        .and_then(|number| number.to_i32())
}

fn dict_cfstring(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<CFString> {
    let key = CFString::new(key);
    dict.find(&key)
        .and_then(|value| value.downcast::<CFString>())
}

fn dict_array(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<CFArray<CFType>> {
    let key = CFString::new(key);
    dict.find(&key)
        .and_then(|value| value.downcast::<CFArray>())
        .map(|array| unsafe { CFArray::<CFType>::wrap_under_get_rule(array.as_concrete_TypeRef()) })
}

fn dict_dictionary(
    dict: &CFDictionary<CFString, CFType>,
    key: &str,
) -> Option<CFDictionary<CFString, CFType>> {
    let key = CFString::new(key);
    dict.find(&key)
        .and_then(|value| value.downcast::<CFDictionary>())
        .map(|dict| unsafe {
            CFDictionary::<CFString, CFType>::wrap_under_get_rule(dict.as_concrete_TypeRef())
        })
}

fn dict_space_id(dict: &CFDictionary<CFString, CFType>) -> Option<u64> {
    let key = CFString::new("ManagedSpaceID");
    dict.find(&key)
        .or_else(|| dict.find(CFString::new("id64")))
        .and_then(|value| value.downcast::<CFNumber>())
        .and_then(|number| number.to_i64())
        .and_then(|number| u64::try_from(number).ok())
}

fn restore(ctx: &WindowContext) -> Result<()> {
    let Some(frame) = restore_frames().lock().remove(&ctx.id) else {
        bail!("no stored macos-window frame for active window");
    };
    set_window_frame(ctx.element, frame)
}

fn toggle_fullscreen(ctx: &WindowContext) -> Result<()> {
    remember_frame(ctx);
    let is_fullscreen = ax_bool(ctx.element, "AXFullScreen").unwrap_or(false);
    set_ax_bool(ctx.element, "AXFullScreen", !is_fullscreen)
}

fn set_window_frame_remembering(ctx: &WindowContext, frame: CGRect) -> Result<()> {
    remember_frame(ctx);
    set_window_frame(ctx.element, clamp_to_screen(frame, ctx.screen))
}

fn remember_frame(ctx: &WindowContext) {
    restore_frames().lock().insert(ctx.id, ctx.frame);
}

fn restore_frames() -> &'static Mutex<HashMap<WindowId, CGRect>> {
    RESTORE_FRAMES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn copy_ax_attr(element: AXUIElementRef, attr: &str) -> Result<CFType> {
    let attr_ref = CFString::new(attr);
    let mut value: CFTypeRef = std::ptr::null();
    let err = unsafe {
        AXUIElementCopyAttributeValue(element, attr_ref.as_concrete_TypeRef(), &mut value)
    };
    if err != K_AX_ERROR_SUCCESS || value.is_null() {
        bail!("AXUIElementCopyAttributeValue({attr}) failed with AXError {err}");
    }
    Ok(unsafe { CFType::wrap_under_create_rule(value) })
}

fn window_frame(window: AXUIElementRef) -> Result<CGRect> {
    Ok(rect_from_parts(
        ax_point(window, "AXPosition")?,
        ax_size(window, "AXSize")?,
    ))
}

fn screen_for_frame(frame: CGRect) -> Result<CGRect> {
    let center = CGPoint::new(
        frame.origin.x + frame.size.width / 2.0,
        frame.origin.y + frame.size.height / 2.0,
    );

    for display in active_display_bounds()? {
        if rect_contains(display, center) {
            return Ok(display);
        }
    }

    Ok(unsafe { CGDisplayBounds(CGMainDisplayID()) })
}

fn active_display_bounds() -> Result<Vec<CGRect>> {
    let mut count = 0;
    let err = unsafe { CGGetActiveDisplayList(0, std::ptr::null_mut(), &mut count) };
    if err != KCG_ERROR_SUCCESS {
        bail!("CGGetActiveDisplayList failed with CGError {err}");
    }

    let mut displays = vec![0; count as usize];
    let err = unsafe { CGGetActiveDisplayList(count, displays.as_mut_ptr(), &mut count) };
    if err != KCG_ERROR_SUCCESS {
        bail!("CGGetActiveDisplayList failed with CGError {err}");
    }

    let mut bounds = displays
        .into_iter()
        .take(count as usize)
        .map(|display| unsafe { CGDisplayBounds(display) })
        .collect::<Vec<_>>();
    bounds.sort_by(|a, b| {
        a.origin
            .x
            .total_cmp(&b.origin.x)
            .then_with(|| a.origin.y.total_cmp(&b.origin.y))
    });
    Ok(bounds)
}

fn ax_point(element: AXUIElementRef, attr: &str) -> Result<CGPoint> {
    let value = copy_ax_attr(element, attr)?;
    let mut point = CGPoint::new(0.0, 0.0);
    let ok = unsafe {
        AXValueGetValue(
            value.as_CFTypeRef(),
            K_AX_VALUE_CGPOINT_TYPE,
            &mut point as *mut CGPoint as *mut c_void,
        )
    };
    ok.then_some(point)
        .ok_or_else(|| anyhow!("AXValueGetValue({attr}) did not contain a CGPoint"))
}

fn ax_size(element: AXUIElementRef, attr: &str) -> Result<CGSize> {
    let value = copy_ax_attr(element, attr)?;
    let mut size = CGSize::new(0.0, 0.0);
    let ok = unsafe {
        AXValueGetValue(
            value.as_CFTypeRef(),
            K_AX_VALUE_CGSIZE_TYPE,
            &mut size as *mut CGSize as *mut c_void,
        )
    };
    ok.then_some(size)
        .ok_or_else(|| anyhow!("AXValueGetValue({attr}) did not contain a CGSize"))
}

fn ax_bool(element: AXUIElementRef, attr: &str) -> Result<bool> {
    let value = copy_ax_attr(element, attr)?;
    value
        .downcast::<CFBoolean>()
        .map(bool::from)
        .ok_or_else(|| anyhow!("AXUIElementCopyAttributeValue({attr}) did not contain a bool"))
}

fn set_window_frame(window: AXUIElementRef, frame: CGRect) -> Result<()> {
    set_ax_size(window, "AXSize", frame.size)?;
    set_ax_point(window, "AXPosition", frame.origin)
}

fn set_ax_point(element: AXUIElementRef, attr: &str, point: CGPoint) -> Result<()> {
    let value = unsafe {
        AXValueCreate(
            K_AX_VALUE_CGPOINT_TYPE,
            &point as *const CGPoint as *const c_void,
        )
    };
    if value.is_null() {
        bail!("AXValueCreate({attr}) returned null");
    }
    let value = unsafe { CFType::wrap_under_create_rule(value) };
    set_ax_attr(element, attr, value.as_CFTypeRef())
}

fn set_ax_size(element: AXUIElementRef, attr: &str, size: CGSize) -> Result<()> {
    let value = unsafe {
        AXValueCreate(
            K_AX_VALUE_CGSIZE_TYPE,
            &size as *const CGSize as *const c_void,
        )
    };
    if value.is_null() {
        bail!("AXValueCreate({attr}) returned null");
    }
    let value = unsafe { CFType::wrap_under_create_rule(value) };
    set_ax_attr(element, attr, value.as_CFTypeRef())
}

fn set_ax_bool(element: AXUIElementRef, attr: &str, value: bool) -> Result<()> {
    let value = CFBoolean::from(value);
    set_ax_attr(element, attr, value.as_CFTypeRef())
}

fn set_ax_attr(element: AXUIElementRef, attr: &str, value: CFTypeRef) -> Result<()> {
    let attr_ref = CFString::new(attr);
    let err =
        unsafe { AXUIElementSetAttributeValue(element, attr_ref.as_concrete_TypeRef(), value) };
    if err != K_AX_ERROR_SUCCESS {
        bail!("AXUIElementSetAttributeValue({attr}) failed with AXError {err}");
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum MoveEdge {
    Left,
    Right,
    Top,
    Bottom,
}

#[derive(Clone, Copy)]
enum DisplayStep {
    Previous,
    Next,
}

#[derive(Clone, Copy)]
enum SpaceStep {
    Previous,
    Next,
}

#[derive(Debug)]
struct ManagedDisplaySpaces {
    display_identifier: String,
    current_space: u64,
    spaces: Vec<u64>,
}

fn move_to_edge(frame: CGRect, screen: CGRect, edge: MoveEdge) -> CGRect {
    match edge {
        MoveEdge::Left => rect(
            screen.origin.x,
            frame.origin.y,
            frame.size.width,
            frame.size.height,
        ),
        MoveEdge::Right => rect(
            screen.origin.x + screen.size.width - frame.size.width,
            frame.origin.y,
            frame.size.width,
            frame.size.height,
        ),
        MoveEdge::Top => rect(
            frame.origin.x,
            screen.origin.y,
            frame.size.width,
            frame.size.height,
        ),
        MoveEdge::Bottom => rect(
            frame.origin.x,
            screen.origin.y + screen.size.height - frame.size.height,
            frame.size.width,
            frame.size.height,
        ),
    }
}

fn move_to_display(ctx: &WindowContext, step: DisplayStep) -> Result<()> {
    let displays = active_display_bounds()?;
    if displays.len() < 2 {
        bail!("macos-window display movement requires at least two active displays");
    }
    let current_idx = displays
        .iter()
        .position(|display| rect_eq(*display, ctx.screen))
        .unwrap_or(0);
    let target_idx = match step {
        DisplayStep::Previous => (current_idx + displays.len() - 1) % displays.len(),
        DisplayStep::Next => (current_idx + 1) % displays.len(),
    };
    let frame = move_frame_between_screens(ctx.frame, displays[current_idx], displays[target_idx]);
    set_window_frame_remembering(ctx, frame)
}

fn switch_space(ctx: &WindowContext, step: SpaceStep) -> Result<()> {
    let (display_identifier, target_space) = target_space_for_window(ctx, step)?;
    let cid = unsafe { CGSMainConnectionID() };
    let display_identifier = CFString::new(&display_identifier);
    let err = unsafe {
        CGSManagedDisplaySetCurrentSpace(
            cid,
            display_identifier.as_concrete_TypeRef(),
            target_space,
        )
    };
    check_cg_error(err, "CGSManagedDisplaySetCurrentSpace")?;
    show_spaces(cid, &[target_space])
}

fn move_window_to_space(ctx: &WindowContext, step: SpaceStep) -> Result<()> {
    let (_, target_space) = target_space_for_window(ctx, step)?;
    let cid = unsafe { CGSMainConnectionID() };
    let window = CFNumber::from(i64::from(ctx.id.0));
    let windows = CFArray::from_CFTypes(&[window]);
    let err =
        unsafe { CGSMoveWindowsToManagedSpace(cid, windows.as_concrete_TypeRef(), target_space) };
    check_cg_error(err, "CGSMoveWindowsToManagedSpace")
}

fn target_space_for_window(ctx: &WindowContext, step: SpaceStep) -> Result<(String, u64)> {
    let cid = unsafe { CGSMainConnectionID() };
    let display_identifier = managed_display_for_window(cid, ctx.id)?;
    let display_spaces = managed_display_spaces(cid)?
        .into_iter()
        .find(|display| display.display_identifier == display_identifier)
        .ok_or_else(|| {
            anyhow!("could not find managed display spaces for display {display_identifier}")
        })?;
    let target_space = step_space(&display_spaces, step)?;
    Ok((display_spaces.display_identifier, target_space))
}

fn step_space(display_spaces: &ManagedDisplaySpaces, step: SpaceStep) -> Result<u64> {
    if display_spaces.spaces.len() < 2 {
        bail!("macos-window space movement requires at least two spaces on the active display");
    }
    let current_index = display_spaces
        .spaces
        .iter()
        .position(|space| *space == display_spaces.current_space)
        .ok_or_else(|| {
            anyhow!(
                "current space {} is not listed in managed display spaces",
                display_spaces.current_space
            )
        })?;
    let target_index = match step {
        SpaceStep::Previous => {
            (current_index + display_spaces.spaces.len() - 1) % display_spaces.spaces.len()
        }
        SpaceStep::Next => (current_index + 1) % display_spaces.spaces.len(),
    };
    Ok(display_spaces.spaces[target_index])
}

fn managed_display_for_window(cid: i32, window: WindowId) -> Result<String> {
    let display = unsafe { CGSCopyManagedDisplayForWindow(cid, window.0) };
    if display.is_null() {
        bail!("CGSCopyManagedDisplayForWindow returned null");
    }
    let display = unsafe { CFString::wrap_under_create_rule(display) };
    Ok(display.to_string())
}

fn managed_display_spaces(cid: i32) -> Result<Vec<ManagedDisplaySpaces>> {
    let displays = unsafe { CGSCopyManagedDisplaySpaces(cid) };
    if displays.is_null() {
        bail!("CGSCopyManagedDisplaySpaces returned null");
    }
    let displays = unsafe { CFArray::<CFType>::wrap_under_create_rule(displays) };
    Ok(parse_managed_display_spaces(&displays))
}

fn parse_managed_display_spaces(displays: &CFArray<CFType>) -> Vec<ManagedDisplaySpaces> {
    let mut parsed_displays = Vec::with_capacity(displays.len() as usize);
    for display in displays.iter() {
        let Some(display) = display.downcast::<CFDictionary>() else {
            continue;
        };
        let display = unsafe {
            CFDictionary::<CFString, CFType>::wrap_under_get_rule(display.as_concrete_TypeRef())
        };
        let Some(display_identifier) = dict_cfstring(&display, "Display Identifier") else {
            continue;
        };
        let Some(current_space) =
            dict_dictionary(&display, "Current Space").and_then(|space| dict_space_id(&space))
        else {
            continue;
        };
        let Some(spaces) = dict_array(&display, "Spaces") else {
            continue;
        };
        let spaces = spaces
            .iter()
            .filter_map(|space| {
                space
                    .downcast::<CFDictionary>()
                    .map(|space| unsafe {
                        CFDictionary::<CFString, CFType>::wrap_under_get_rule(
                            space.as_concrete_TypeRef(),
                        )
                    })
                    .and_then(|space| dict_space_id(&space))
            })
            .collect::<Vec<_>>();
        parsed_displays.push(ManagedDisplaySpaces {
            display_identifier: display_identifier.to_string(),
            current_space,
            spaces,
        });
    }
    parsed_displays
}

fn show_spaces(cid: i32, spaces: &[u64]) -> Result<()> {
    let spaces = spaces
        .iter()
        .map(|space| {
            i64::try_from(*space)
                .map(CFNumber::from)
                .map_err(|_| anyhow!("space id {space} does not fit in a CFNumber SInt64"))
        })
        .collect::<Result<Vec<_>>>()?;
    let spaces = CFArray::from_CFTypes(&spaces);
    let err = unsafe { CGSShowSpaces(cid, spaces.as_concrete_TypeRef()) };
    check_cg_error(err, "CGSShowSpaces")
}

fn check_cg_error(err: CGError, label: &str) -> Result<()> {
    if err != KCG_ERROR_SUCCESS {
        bail!("{label} failed with CGError {err}");
    }
    Ok(())
}

fn move_frame_between_screens(frame: CGRect, source: CGRect, target: CGRect) -> CGRect {
    let rel_x = (frame.origin.x - source.origin.x) / source.size.width;
    let rel_y = (frame.origin.y - source.origin.y) / source.size.height;
    let rel_w = frame.size.width / source.size.width;
    let rel_h = frame.size.height / source.size.height;
    rect(
        target.origin.x + target.size.width * rel_x,
        target.origin.y + target.size.height * rel_y,
        target.size.width * rel_w,
        target.size.height * rel_h,
    )
}

fn reasonable_size(screen: CGRect) -> CGRect {
    let width = (screen.size.width * 0.6).min(1024.0);
    let height = (screen.size.height * 0.6).min(900.0);
    center_size(screen, width, height)
}

fn center_current_size(screen: CGRect, frame: CGRect) -> CGRect {
    center_size(
        screen,
        frame.size.width.min(screen.size.width),
        frame.size.height.min(screen.size.height),
    )
}

fn resize_from_center(frame: CGRect, screen: CGRect, delta: f64) -> CGRect {
    let width = (frame.size.width + delta * 2.0).clamp(MIN_WINDOW_EDGE_PX, screen.size.width);
    let height = (frame.size.height + delta * 2.0).clamp(MIN_WINDOW_EDGE_PX, screen.size.height);
    let center = CGPoint::new(
        frame.origin.x + frame.size.width / 2.0,
        frame.origin.y + frame.size.height / 2.0,
    );
    rect(
        center.x - width / 2.0,
        center.y - height / 2.0,
        width,
        height,
    )
}

fn center_size(screen: CGRect, width: f64, height: f64) -> CGRect {
    rect(
        screen.origin.x + (screen.size.width - width) / 2.0,
        screen.origin.y + (screen.size.height - height) / 2.0,
        width,
        height,
    )
}

fn resolve_frame(screen: CGRect, frame: MacosWindowFrame) -> CGRect {
    rect(
        screen.origin.x + screen.size.width * f64::from(frame.x) / BASIS_POINTS,
        screen.origin.y + screen.size.height * f64::from(frame.y) / BASIS_POINTS,
        screen.size.width * f64::from(frame.width) / BASIS_POINTS,
        screen.size.height * f64::from(frame.height) / BASIS_POINTS,
    )
}

fn clamp_to_screen(frame: CGRect, screen: CGRect) -> CGRect {
    let width = frame
        .size
        .width
        .clamp(MIN_WINDOW_EDGE_PX, screen.size.width);
    let height = frame
        .size
        .height
        .clamp(MIN_WINDOW_EDGE_PX, screen.size.height);
    let max_x = screen.origin.x + screen.size.width - width;
    let max_y = screen.origin.y + screen.size.height - height;
    rect(
        frame.origin.x.clamp(screen.origin.x, max_x),
        frame.origin.y.clamp(screen.origin.y, max_y),
        width,
        height,
    )
}

fn rect_contains(rect: CGRect, point: CGPoint) -> bool {
    point.x >= rect.origin.x
        && point.x <= rect.origin.x + rect.size.width
        && point.y >= rect.origin.y
        && point.y <= rect.origin.y + rect.size.height
}

fn rect_eq(a: CGRect, b: CGRect) -> bool {
    (a.origin.x - b.origin.x).abs() < f64::EPSILON
        && (a.origin.y - b.origin.y).abs() < f64::EPSILON
        && (a.size.width - b.size.width).abs() < f64::EPSILON
        && (a.size.height - b.size.height).abs() < f64::EPSILON
}

fn rect_from_parts(origin: CGPoint, size: CGSize) -> CGRect {
    CGRect::new(&origin, &size)
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> CGRect {
    CGRect::new(&CGPoint::new(x, y), &CGSize::new(width, height))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_rect_eq(actual: CGRect, expected: CGRect) {
        assert!(
            (actual.origin.x - expected.origin.x).abs() < f64::EPSILON,
            "x: actual {}, expected {}",
            actual.origin.x,
            expected.origin.x
        );
        assert!(
            (actual.origin.y - expected.origin.y).abs() < f64::EPSILON,
            "y: actual {}, expected {}",
            actual.origin.y,
            expected.origin.y
        );
        assert!(
            (actual.size.width - expected.size.width).abs() < f64::EPSILON,
            "width: actual {}, expected {}",
            actual.size.width,
            expected.size.width
        );
        assert!(
            (actual.size.height - expected.size.height).abs() < f64::EPSILON,
            "height: actual {}, expected {}",
            actual.size.height,
            expected.size.height
        );
    }

    #[test]
    fn resolves_basis_point_frames_relative_to_screen() {
        let screen = rect(100.0, 50.0, 1000.0, 800.0);
        let actual = resolve_frame(screen, bp(2500, 1250, 5000, 7500));
        assert_rect_eq(actual, rect(350.0, 150.0, 500.0, 600.0));
    }

    #[test]
    fn clamps_frames_to_screen_and_minimum_size() {
        let screen = rect(100.0, 50.0, 1000.0, 800.0);

        assert_rect_eq(
            clamp_to_screen(rect(0.0, 0.0, 20.0, 30.0), screen),
            rect(100.0, 50.0, 64.0, 64.0),
        );
        assert_rect_eq(
            clamp_to_screen(rect(1200.0, 900.0, 600.0, 500.0), screen),
            rect(500.0, 350.0, 600.0, 500.0),
        );
    }

    #[test]
    fn moves_frames_between_displays_by_relative_geometry() {
        let source = rect(0.0, 0.0, 1000.0, 800.0);
        let target = rect(1000.0, -200.0, 2000.0, 1200.0);
        let frame = rect(100.0, 200.0, 500.0, 400.0);

        assert_rect_eq(
            move_frame_between_screens(frame, source, target),
            rect(1200.0, 100.0, 1000.0, 600.0),
        );
    }

    #[test]
    fn moves_existing_frame_to_requested_screen_edge() {
        let screen = rect(100.0, 50.0, 1000.0, 800.0);
        let frame = rect(300.0, 250.0, 400.0, 300.0);

        assert_rect_eq(
            move_to_edge(frame, screen, MoveEdge::Left),
            rect(100.0, 250.0, 400.0, 300.0),
        );
        assert_rect_eq(
            move_to_edge(frame, screen, MoveEdge::Right),
            rect(700.0, 250.0, 400.0, 300.0),
        );
        assert_rect_eq(
            move_to_edge(frame, screen, MoveEdge::Top),
            rect(300.0, 50.0, 400.0, 300.0),
        );
        assert_rect_eq(
            move_to_edge(frame, screen, MoveEdge::Bottom),
            rect(300.0, 550.0, 400.0, 300.0),
        );
    }

    #[test]
    fn resizing_from_center_respects_screen_limits() {
        let screen = rect(0.0, 0.0, 500.0, 400.0);

        assert_rect_eq(
            resize_from_center(rect(100.0, 100.0, 100.0, 100.0), screen, -64.0),
            rect(118.0, 118.0, 64.0, 64.0),
        );
        assert_rect_eq(
            resize_from_center(rect(100.0, 100.0, 200.0, 100.0), screen, 500.0),
            rect(-50.0, -50.0, 500.0, 400.0),
        );
    }

    #[test]
    fn steps_spaces_with_wraparound() {
        let display_spaces = ManagedDisplaySpaces {
            display_identifier: "Main".to_owned(),
            current_space: 20,
            spaces: vec![10, 20, 30],
        };

        assert_eq!(
            step_space(&display_spaces, SpaceStep::Previous).unwrap(),
            10
        );
        assert_eq!(step_space(&display_spaces, SpaceStep::Next).unwrap(), 30);

        let display_spaces = ManagedDisplaySpaces {
            display_identifier: "Main".to_owned(),
            current_space: 10,
            spaces: vec![10, 20, 30],
        };
        assert_eq!(
            step_space(&display_spaces, SpaceStep::Previous).unwrap(),
            30
        );
    }

    #[test]
    fn rejects_unusable_space_lists() {
        assert!(
            step_space(
                &ManagedDisplaySpaces {
                    display_identifier: "Main".to_owned(),
                    current_space: 10,
                    spaces: vec![10],
                },
                SpaceStep::Next,
            )
            .is_err()
        );
        assert!(
            step_space(
                &ManagedDisplaySpaces {
                    display_identifier: "Main".to_owned(),
                    current_space: 40,
                    spaces: vec![10, 20],
                },
                SpaceStep::Next,
            )
            .is_err()
        );
    }

    #[test]
    fn parses_managed_display_spaces_from_core_graphics_shape() {
        let display = cf_display_spaces(
            "Main",
            cf_space("ManagedSpaceID", 20),
            &[
                cf_space("ManagedSpaceID", 10),
                cf_space("id64", 20),
                cf_space("ManagedSpaceID", 30),
            ],
        );
        let displays = CFArray::from_CFTypes(&[display]);

        let parsed = parse_managed_display_spaces(&displays);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].display_identifier, "Main");
        assert_eq!(parsed[0].current_space, 20);
        assert_eq!(parsed[0].spaces, vec![10, 20, 30]);
    }

    fn cf_display_spaces(
        display_identifier: &str,
        current_space: CFType,
        spaces: &[CFType],
    ) -> CFType {
        let spaces = CFArray::from_CFTypes(spaces).as_CFType();
        CFDictionary::from_CFType_pairs(&[
            (
                CFString::new("Display Identifier").as_CFType(),
                CFString::new(display_identifier).as_CFType(),
            ),
            (CFString::new("Current Space").as_CFType(), current_space),
            (CFString::new("Spaces").as_CFType(), spaces),
        ])
        .as_CFType()
    }

    fn cf_space(key: &str, id: i64) -> CFType {
        CFDictionary::from_CFType_pairs(&[(
            CFString::new(key).as_CFType(),
            CFNumber::from(id).as_CFType(),
        )])
        .as_CFType()
    }
}
