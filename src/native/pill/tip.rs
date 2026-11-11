//! A capsule button's tooltip: what it does and the keys that do the same (`model::tip_of`), in a
//! small panel of its own under the capsule (or above it near the bottom of the screen). It's
//! drawn by AppKit rather than in the capsule's Metal panel so it can sit outside the capsule,
//! and AppKit's own tooltips never show while Scrawl isn't the active app, which it never is
//! while recording. Like the capsule it stays ordered in, invisible when no tooltip shows, so
//! the capture's filter always has it to leave out.

use std::time::Instant;

use objc2::MainThreadMarker;
use objc2::rc::Retained;
use objc2_app_kit::{
    NSBackingStoreType, NSBox, NSBoxType, NSColor, NSFont, NSFontWeightMedium, NSFontWeightRegular,
    NSPanel, NSScreen, NSStatusWindowLevel, NSTextField, NSTitlePosition, NSView,
    NSWindowCollectionBehavior, NSWindowStyleMask,
};
use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};

use super::model::ElemId;

/// How long the pointer rests on a button before its tooltip shows, and for how long after one
/// hides the next button's shows at once, as macOS's own do.
const DELAY_S: f64 = 0.5;
const HANDOFF_S: f64 = 0.4;
const FADE_IN_S: f32 = 0.10;
const FADE_OUT_S: f32 = 0.08;
/// Its height, its distance from the capsule and from the screen's edges, its text's inset,
/// and the space between the name and the keys.
const H: f64 = 24.0;
const GAP: f64 = 6.0;
const MARGIN: f64 = 4.0;
const PX: f64 = 9.0;
const KEYS_GAP: f64 = 6.0;
const FONT: f64 = 12.0;

/// Where a tooltip goes, in global points (top-left origin).
#[derive(Clone, Copy, Debug)]
pub(super) struct Anchor {
    /// The button's horizontal centre.
    pub x: f64,
    /// The capsule's top and bottom edges.
    pub top: f64,
    pub bottom: f64,
    /// The display's visible frame.
    pub visible: [f64; 4],
}

/// What a tooltip says: the button's name and its keys (empty: none).
pub(super) type Text = (&'static str, String);

pub(super) struct Tip {
    window: Retained<NSPanel>,
    back: Retained<NSBox>,
    name: Retained<NSTextField>,
    keys: Retained<NSTextField>,
    clock: Instant,
    /// The button under the pointer, and since when.
    hover: Option<(ElemId, f64)>,
    /// What the tooltip shows now.
    shown: Option<(ElemId, Text)>,
    hidden_at: f64,
    /// The button just pressed: its tooltip stays away until the pointer leaves it.
    muted: Option<ElemId>,
    last_update: f64,
    alpha: f32,
    frame: [f64; 4],
}

impl Tip {
    pub fn open(mtm: MainThreadMarker) -> Self {
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(80.0, H));
        let window = NSPanel::initWithContentRect_styleMask_backing_defer(
            mtm.alloc(),
            rect,
            NSWindowStyleMask::Borderless | NSWindowStyleMask::NonactivatingPanel,
            NSBackingStoreType::Buffered,
            false,
        );
        unsafe { window.setReleasedWhenClosed(false) };
        window.setBackgroundColor(Some(&NSColor::clearColor()));
        window.setOpaque(false);
        window.setHasShadow(true);
        window.setIgnoresMouseEvents(true);
        window.setHidesOnDeactivate(false);
        window.setCanHide(false);
        window.setLevel(NSStatusWindowLevel + 6);
        window.setCollectionBehavior(
            NSWindowCollectionBehavior::CanJoinAllSpaces
                | NSWindowCollectionBehavior::CanJoinAllApplications
                | NSWindowCollectionBehavior::FullScreenAuxiliary
                | NSWindowCollectionBehavior::Stationary
                | NSWindowCollectionBehavior::IgnoresCycle,
        );
        window.setAlphaValue(0.0);

        let root = NSView::initWithFrame(mtm.alloc(), rect);
        let back = NSBox::initWithFrame(mtm.alloc(), rect);
        back.setBoxType(NSBoxType::Custom);
        back.setTitlePosition(NSTitlePosition::NoTitle);
        back.setFillColor(&NSColor::colorWithWhite_alpha(0.07, 0.94));
        back.setBorderColor(&NSColor::colorWithWhite_alpha(1.0, 0.16));
        back.setBorderWidth(0.5);
        back.setCornerRadius(7.0);
        root.addSubview(&back);
        let label = |weight, white: f64| {
            let field = NSTextField::labelWithString(&NSString::from_str(""), mtm);
            field.setFont(Some(&NSFont::systemFontOfSize_weight(FONT, weight)));
            field.setTextColor(Some(&NSColor::colorWithWhite_alpha(1.0, white)));
            root.addSubview(&field);
            field
        };
        let name = label(unsafe { NSFontWeightMedium }, 0.95);
        let keys = label(unsafe { NSFontWeightRegular }, 0.58);
        window.setContentView(Some(&root));
        window.orderFrontRegardless();
        Self {
            window,
            back,
            name,
            keys,
            clock: Instant::now(),
            hover: None,
            shown: None,
            hidden_at: f64::NEG_INFINITY,
            muted: None,
            last_update: 0.0,
            alpha: 0.0,
            frame: [0.0; 4],
        }
    }

    pub fn window_number(&self) -> u32 {
        self.window.windowNumber().max(0) as u32
    }

    /// With the capsule's (on the main thread).
    pub fn close(&self) {
        self.window.orderOut(None);
        self.window.close();
    }

    /// Back in front with the capsule when it shows again.
    pub fn order_front(&self) {
        self.window.setLevel(NSStatusWindowLevel + 6);
        self.window.orderFrontRegardless();
    }

    /// Called on every tick with the button under the pointer, its tooltip and where it goes
    /// (`None`: no button, or one without a tooltip), and whether the mouse button is down.
    pub fn update(
        &mut self,
        mtm: MainThreadMarker,
        want: Option<(ElemId, Text, Anchor)>,
        pressed: bool,
    ) {
        let now = self.clock.elapsed().as_secs_f64();
        let dt = (now - self.last_update).clamp(0.0, 0.1) as f32;
        self.last_update = now;
        let on = want.as_ref().map(|(id, ..)| *id);
        if pressed {
            self.muted = on.or(self.muted);
        } else if self.muted != on {
            self.muted = None;
        }
        let want = want.filter(|(id, ..)| !pressed && self.muted != Some(*id));
        let Some((id, text, anchor)) = want else {
            self.hover = None;
            self.hide(now);
            self.fade(dt);
            return;
        };
        if self.hover.is_none_or(|(on, _)| on != id) {
            self.hide(now);
            self.hover = Some((id, now));
        }
        let since = self.hover.map_or(now, |(_, t)| t);
        let due = now - since >= DELAY_S || now - self.hidden_at < HANDOFF_S;
        if due && self.shown.as_ref().is_none_or(|(_, shown)| *shown != text) {
            self.set_text(&text);
            self.shown = Some((id, text));
        }
        if self.shown.is_some() {
            self.place(mtm, anchor);
        }
        self.fade(dt);
    }

    fn hide(&mut self, now: f64) {
        if self.shown.take().is_some() {
            self.hidden_at = now;
        }
    }

    fn fade(&mut self, dt: f32) {
        let (target, s) = if self.shown.is_some() {
            (1.0, FADE_IN_S)
        } else {
            (0.0, FADE_OUT_S)
        };
        let next = if target > self.alpha {
            (self.alpha + dt / s).min(target)
        } else {
            (self.alpha - dt / s).max(target)
        };
        if (next - self.alpha).abs() > 1e-4 {
            self.alpha = next;
            self.window.setAlphaValue(f64::from(next));
        }
    }

    fn set_text(&mut self, (name, keys): &Text) {
        self.name.setStringValue(&NSString::from_str(name));
        self.keys.setStringValue(&NSString::from_str(keys));
        self.name.sizeToFit();
        self.keys.sizeToFit();
        let name_size = self.name.frame().size;
        let keys_size = self.keys.frame().size;
        let keys_w = if keys.is_empty() {
            0.0
        } else {
            KEYS_GAP + keys_size.width
        };
        let w = (PX + name_size.width + keys_w + PX).round();
        let centred = |h: f64| ((H - h) * 0.5).round();
        self.name
            .setFrameOrigin(NSPoint::new(PX, centred(name_size.height)));
        self.keys.setFrameOrigin(NSPoint::new(
            PX + name_size.width + KEYS_GAP,
            centred(keys_size.height),
        ));
        self.keys.setHidden(keys.is_empty());
        self.back
            .setFrame(NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(w, H)));
        self.window.setContentSize(NSSize::new(w, H));
    }

    /// Under the capsule, centred on the button and kept on screen; above it when there's no
    /// room below.
    fn place(&mut self, mtm: MainThreadMarker, anchor: Anchor) {
        let w = self.back.frame().size.width;
        let [vx, vy, vw, vh] = anchor.visible;
        let x = (anchor.x - w * 0.5)
            .min(vx + vw - MARGIN - w)
            .max(vx + MARGIN);
        let below = anchor.bottom + GAP;
        let y = if below + H > vy + vh - MARGIN {
            anchor.top - GAP - H
        } else {
            below
        };
        let frame = [x.round(), y.round(), w, H];
        if frame == self.frame {
            return;
        }
        self.frame = frame;
        let primary_h = NSScreen::screens(mtm)
            .firstObject()
            .map_or(900.0, |s| s.frame().size.height);
        let [ax, ay, aw, ah] = crate::screens::flip(frame, primary_h);
        self.window
            .setFrame_display(NSRect::new(NSPoint::new(ax, ay), NSSize::new(aw, ah)), true);
    }
}
