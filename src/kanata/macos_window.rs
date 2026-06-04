use anyhow::{Result, anyhow, bail};
use core_foundation::array::{CFArray, CFArrayRef};
use core_foundation::base::{CFType, CFTypeRef, TCFType};
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::display::{CGPoint, CGRect, CGSize};
use kanata_parser::custom_action::{MacosWindowFrame, MacosWindowLayout, MacosWindowPreset};
use objc::runtime::Object;
use objc::{Encode, Encoding, class, msg_send, sel, sel_impl};
use parking_lot::Mutex;
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

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct NSPoint {
    x: f64,
    y: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct NSSize {
    width: f64,
    height: f64,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct NSRect {
    origin: NSPoint,
    size: NSSize,
}

unsafe impl Encode for NSRect {
    fn encode() -> Encoding {
        unsafe { Encoding::from_str("{CGRect={CGPoint=dd}{CGSize=dd}}") }
    }
}

#[derive(Clone, Copy, Debug)]
struct Screen {
    full_frame: CGRect,
    work_area: CGRect,
}

#[link(name = "ApplicationServices", kind = "framework")]
unsafe extern "C" {
    fn AXUIElementCreateApplication(pid: i32) -> AXUIElementRef;
    fn AXUIElementCreateSystemWide() -> AXUIElementRef;
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
}

#[link(name = "AppKit", kind = "framework")]
unsafe extern "C" {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct CycleKey {
    layouts_ptr: usize,
    layouts_len: usize,
}

#[derive(Debug)]
struct CycleState {
    key: Option<CycleKey>,
    step: usize,
    last_used: Instant,
}

static CYCLE_STATE: OnceLock<Mutex<CycleState>> = OnceLock::new();

pub fn apply_window_layouts(layouts: &'static [MacosWindowLayout]) -> Result<()> {
    if layouts.is_empty() {
        bail!("macos-window action has no layouts");
    }

    let window = focused_window()?;
    let current_frame = window_frame(window.element)?;
    let screen = screen_for_rect(current_frame)?;
    let key = CycleKey {
        layouts_ptr: layouts.as_ptr() as usize,
        layouts_len: layouts.len(),
    };
    let frame = resolve_layout(screen, current_frame, layouts[next_cycle_index(key)]);
    set_window_frame(window.element, frame)
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
        state.step = (state.step + 1) % key.layouts_len;
    } else {
        state.key = Some(key);
        state.step = 0;
    }
    state.last_used = now;
    state.step
}

struct FocusedWindow {
    _app: CFType,
    _window: CFType,
    element: AXUIElementRef,
}

fn focused_window() -> Result<FocusedWindow> {
    let app = focused_application().or_else(|_| application_for_pid(frontmost_window_pid()?))?;
    let app_element = app.as_CFTypeRef() as AXUIElementRef;
    let window = copy_ax_attr(app_element, "AXFocusedWindow")
        .or_else(|_| copy_ax_attr(app_element, "AXMainWindow"))?;
    let element = window.as_CFTypeRef() as AXUIElementRef;
    Ok(FocusedWindow {
        _app: app,
        _window: window,
        element,
    })
}

fn focused_application() -> Result<CFType> {
    let system = unsafe { AXUIElementCreateSystemWide() };
    if system.is_null() {
        bail!("AXUIElementCreateSystemWide returned null");
    }
    let system = unsafe { CFType::wrap_under_create_rule(system as CFTypeRef) };
    copy_ax_attr(
        system.as_CFTypeRef() as AXUIElementRef,
        "AXFocusedApplication",
    )
}

fn application_for_pid(pid: i32) -> Result<CFType> {
    let app = unsafe { AXUIElementCreateApplication(pid) };
    if app.is_null() {
        bail!("AXUIElementCreateApplication({pid}) returned null");
    }
    Ok(unsafe { CFType::wrap_under_create_rule(app as CFTypeRef) })
}

fn frontmost_window_pid() -> Result<i32> {
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
        if layer == Some(0)
            && let Some(pid) = pid
        {
            return Ok(pid);
        }
    }
    bail!("could not find a frontmost on-screen window PID")
}

fn dict_i32(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<i32> {
    let key = CFString::new(key);
    dict.find(&key)
        .and_then(|value| value.downcast::<CFNumber>())
        .and_then(|number| number.to_i32())
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
    let position = ax_point(window, "AXPosition")?;
    let size = ax_size(window, "AXSize")?;
    Ok(CGRect::new(&position, &size))
}

fn screen_for_rect(rect: CGRect) -> Result<Screen> {
    let center = CGPoint::new(
        rect.origin.x + rect.size.width / 2.0,
        rect.origin.y + rect.size.height / 2.0,
    );

    for screen in active_screens()? {
        if rect_contains(screen.full_frame, center) {
            return Ok(screen);
        }
    }

    let main = unsafe { CGDisplayBounds(CGMainDisplayID()) };
    Ok(Screen {
        full_frame: main,
        work_area: main,
    })
}

fn active_screens() -> Result<Vec<Screen>> {
    let screens = appkit_visible_screens();
    if !screens.is_empty() {
        return Ok(screens);
    }

    Ok(active_display_bounds()?
        .into_iter()
        .map(|display| Screen {
            full_frame: display,
            work_area: display,
        })
        .collect())
}

fn appkit_visible_screens() -> Vec<Screen> {
    unsafe {
        let pool: *mut Object = msg_send![class!(NSAutoreleasePool), new];
        let screens: *mut Object = msg_send![class!(NSScreen), screens];
        if screens.is_null() {
            let _: () = msg_send![pool, drain];
            return Vec::new();
        }

        let key: *mut Object =
            msg_send![class!(NSString), stringWithUTF8String: c"NSScreenNumber".as_ptr()];
        let count: usize = msg_send![screens, count];
        let mut result = Vec::with_capacity(count);

        for index in 0..count {
            let screen: *mut Object = msg_send![screens, objectAtIndex: index];
            if screen.is_null() {
                continue;
            }

            let description: *mut Object = msg_send![screen, deviceDescription];
            let number: *mut Object = msg_send![description, objectForKey: key];
            if number.is_null() {
                continue;
            }

            let display_id: u32 = msg_send![number, unsignedIntValue];
            let full_frame = CGDisplayBounds(display_id);
            let ns_frame: NSRect = msg_send![screen, frame];
            let visible_frame: NSRect = msg_send![screen, visibleFrame];
            let work_area = appkit_rect_to_ax_rect(ns_frame, visible_frame, full_frame);
            result.push(Screen {
                full_frame,
                work_area,
            });
        }

        let _: () = msg_send![pool, drain];
        result
    }
}

fn appkit_rect_to_ax_rect(ns_frame: NSRect, visible_frame: NSRect, cg_frame: CGRect) -> CGRect {
    let left_inset = visible_frame.origin.x - ns_frame.origin.x;
    let top_inset = ns_frame.origin.y + ns_frame.size.height
        - visible_frame.origin.y
        - visible_frame.size.height;

    CGRect::new(
        &CGPoint::new(
            cg_frame.origin.x + left_inset,
            cg_frame.origin.y + top_inset,
        ),
        &CGSize::new(visible_frame.size.width, visible_frame.size.height),
    )
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

    Ok(displays
        .into_iter()
        .take(count as usize)
        .map(|display| unsafe { CGDisplayBounds(display) })
        .collect())
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

fn rect_contains(rect: CGRect, point: CGPoint) -> bool {
    point.x >= rect.origin.x
        && point.x <= rect.origin.x + rect.size.width
        && point.y >= rect.origin.y
        && point.y <= rect.origin.y + rect.size.height
}

fn resolve_layout(screen: Screen, current_frame: CGRect, layout: MacosWindowLayout) -> CGRect {
    match layout {
        MacosWindowLayout::Frame(frame) => resolve_frame(screen.work_area, frame),
        MacosWindowLayout::Preset(preset) => resolve_preset(screen, current_frame, preset),
    }
}

fn resolve_preset(screen: Screen, current_frame: CGRect, preset: MacosWindowPreset) -> CGRect {
    use MacosWindowPreset::*;
    let work_area = screen.work_area;
    match preset {
        Maximize => grid_rect(work_area, 0, 0, 1, 1),
        AlmostMaximize => resolve_frame(
            work_area,
            MacosWindowFrame {
                x: 500,
                y: 500,
                width: 9_000,
                height: 9_000,
            },
        ),
        LeftHalf => grid_rect(work_area, 0, 0, 2, 1),
        RightHalf => grid_rect(work_area, 1, 0, 2, 1),
        TopHalf => grid_rect(work_area, 0, 0, 1, 2),
        BottomHalf => grid_rect(work_area, 0, 1, 1, 2),
        Center => center_current_window(work_area, current_frame),
        FirstThird => grid_rect(work_area, 0, 0, 3, 1),
        CenterThird => grid_rect(work_area, 1, 0, 3, 1),
        LastThird => grid_rect(work_area, 2, 0, 3, 1),
        LeftTwoThirds => span_rect(work_area, 0, 0, 2, 1, 3, 1),
        CenterTwoThirds => span_rect(work_area, 0.5, 0.0, 2.0, 1.0, 3.0, 1.0),
        RightTwoThirds => span_rect(work_area, 1, 0, 2, 1, 3, 1),
        FirstThreeFourths => span_rect(work_area, 0, 0, 3, 1, 4, 1),
        CenterThreeFourths => span_rect(work_area, 0.5, 0.0, 3.0, 1.0, 4.0, 1.0),
        LastThreeFourths => span_rect(work_area, 1, 0, 3, 1, 4, 1),
        TopThird => grid_rect(work_area, 0, 0, 1, 3),
        MiddleThird => grid_rect(work_area, 0, 1, 1, 3),
        BottomThird => grid_rect(work_area, 0, 2, 1, 3),
        TopTwoThirds => span_rect(work_area, 0, 0, 1, 2, 1, 3),
        BottomTwoThirds => span_rect(work_area, 0, 1, 1, 2, 1, 3),
        TopCenterTwoThirds => span_rect(work_area, 0.5, 0.0, 2.0, 1.0, 3.0, 2.0),
        BottomCenterTwoThirds => span_rect(work_area, 0.5, 1.0, 2.0, 1.0, 3.0, 2.0),
        TopFirstFourth => span_rect(work_area, 0, 0, 1, 1, 4, 2),
        TopSecondFourth => span_rect(work_area, 1, 0, 1, 1, 4, 2),
        TopThirdFourth => span_rect(work_area, 2, 0, 1, 1, 4, 2),
        TopLastFourth => span_rect(work_area, 3, 0, 1, 1, 4, 2),
        TopThreeFourths => span_rect(work_area, 0, 0, 1, 3, 1, 4),
        BottomThreeFourths => span_rect(work_area, 0, 1, 1, 3, 1, 4),
        FirstFourth => grid_rect(work_area, 0, 0, 4, 1),
        SecondFourth => grid_rect(work_area, 1, 0, 4, 1),
        ThirdFourth => grid_rect(work_area, 2, 0, 4, 1),
        LastFourth => grid_rect(work_area, 3, 0, 4, 1),
        TopLeftQuarter => grid_rect(work_area, 0, 0, 2, 2),
        TopRightQuarter => grid_rect(work_area, 1, 0, 2, 2),
        BottomLeftQuarter => grid_rect(work_area, 0, 1, 2, 2),
        BottomRightQuarter => grid_rect(work_area, 1, 1, 2, 2),
        TopLeftSixth => grid_rect(work_area, 0, 0, 3, 2),
        TopCenterSixth => grid_rect(work_area, 1, 0, 3, 2),
        TopRightSixth => grid_rect(work_area, 2, 0, 3, 2),
        BottomLeftSixth => grid_rect(work_area, 0, 1, 3, 2),
        BottomCenterSixth => grid_rect(work_area, 1, 1, 3, 2),
        BottomRightSixth => grid_rect(work_area, 2, 1, 3, 2),
        MoveLeft => move_current_window(work_area, current_frame, MoveEdge::Left),
        MoveRight => move_current_window(work_area, current_frame, MoveEdge::Right),
        MoveTop => move_current_window(work_area, current_frame, MoveEdge::Top),
        MoveBottom => move_current_window(work_area, current_frame, MoveEdge::Bottom),
    }
}

fn center_current_window(screen: CGRect, current_frame: CGRect) -> CGRect {
    let width = current_frame.size.width.min(screen.size.width);
    let height = current_frame.size.height.min(screen.size.height);
    CGRect::new(
        &CGPoint::new(
            screen.origin.x + (screen.size.width - width) / 2.0,
            screen.origin.y + (screen.size.height - height) / 2.0,
        ),
        &CGSize::new(width, height),
    )
}

enum MoveEdge {
    Left,
    Right,
    Top,
    Bottom,
}

fn move_current_window(screen: CGRect, current_frame: CGRect, edge: MoveEdge) -> CGRect {
    let width = current_frame.size.width.min(screen.size.width);
    let height = current_frame.size.height.min(screen.size.height);
    let x = match edge {
        MoveEdge::Left => screen.origin.x,
        MoveEdge::Right => screen.origin.x + screen.size.width - width,
        MoveEdge::Top | MoveEdge::Bottom => clamp_axis(
            current_frame.origin.x,
            screen.origin.x,
            screen.size.width,
            width,
        ),
    };
    let y = match edge {
        MoveEdge::Top => screen.origin.y,
        MoveEdge::Bottom => screen.origin.y + screen.size.height - height,
        MoveEdge::Left | MoveEdge::Right => clamp_axis(
            current_frame.origin.y,
            screen.origin.y,
            screen.size.height,
            height,
        ),
    };
    CGRect::new(&CGPoint::new(x, y), &CGSize::new(width, height))
}

fn clamp_axis(value: f64, origin: f64, span: f64, size: f64) -> f64 {
    if size >= span {
        return origin;
    }
    value.max(origin).min(origin + span - size)
}

fn resolve_frame(screen: CGRect, frame: MacosWindowFrame) -> CGRect {
    CGRect::new(
        &CGPoint::new(
            screen.origin.x + screen.size.width * f64::from(frame.x) / BASIS_POINTS,
            screen.origin.y + screen.size.height * f64::from(frame.y) / BASIS_POINTS,
        ),
        &CGSize::new(
            screen.size.width * f64::from(frame.width) / BASIS_POINTS,
            screen.size.height * f64::from(frame.height) / BASIS_POINTS,
        ),
    )
}

fn grid_rect(screen: CGRect, column: u32, row: u32, columns: u32, rows: u32) -> CGRect {
    span_rect(
        screen,
        f64::from(column),
        f64::from(row),
        1.0,
        1.0,
        f64::from(columns),
        f64::from(rows),
    )
}

fn span_rect(
    screen: CGRect,
    column: impl Into<f64>,
    row: impl Into<f64>,
    column_span: impl Into<f64>,
    row_span: impl Into<f64>,
    columns: impl Into<f64>,
    rows: impl Into<f64>,
) -> CGRect {
    let column = column.into();
    let row = row.into();
    let column_span = column_span.into();
    let row_span = row_span.into();
    let columns = columns.into();
    let rows = rows.into();
    CGRect::new(
        &CGPoint::new(
            screen.origin.x + screen.size.width * column / columns,
            screen.origin.y + screen.size.height * row / rows,
        ),
        &CGSize::new(
            screen.size.width * column_span / columns,
            screen.size.height * row_span / rows,
        ),
    )
}

fn set_window_frame(window: AXUIElementRef, frame: CGRect) -> Result<()> {
    set_ax_size(window, "AXSize", frame.size)?;
    set_ax_point(window, "AXPosition", frame.origin)?;
    let actual = window_frame(window)?;
    if !rects_nearly_equal(actual, frame) {
        set_ax_size(window, "AXSize", frame.size)?;
        set_ax_point(window, "AXPosition", frame.origin)?;
    }
    Ok(())
}

fn rects_nearly_equal(left: CGRect, right: CGRect) -> bool {
    (left.origin.x - right.origin.x).abs() <= 1.0
        && (left.origin.y - right.origin.y).abs() <= 1.0
        && (left.size.width - right.size.width).abs() <= 1.0
        && (left.size.height - right.size.height).abs() <= 1.0
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

fn set_ax_attr(element: AXUIElementRef, attr: &str, value: CFTypeRef) -> Result<()> {
    let attr_ref = CFString::new(attr);
    let err =
        unsafe { AXUIElementSetAttributeValue(element, attr_ref.as_concrete_TypeRef(), value) };
    if err != K_AX_ERROR_SUCCESS {
        bail!("AXUIElementSetAttributeValue({attr}) failed with AXError {err}");
    }
    Ok(())
}
