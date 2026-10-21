//! Look and feel: the embedded Vietnamese font, large text, light/dark colors, and the
//! hand-drawn icons, big buttons, cards and pictures the screens are built from.
//! Icons are painted on a 24×24 grid so they stay sharp at any size and need no image files.

use std::sync::Arc;

use eframe::egui::{
    self, pos2, vec2, Align2, Color32, CornerRadius, FontFamily, FontId, Painter, Rect, Response, RichText, Sense, Shape, Stroke,
    StrokeKind, Theme, Ui, Vec2,
};

/// Be Vietnamese Pro (SIL Open Font License 1.1, assets/fonts/OFL.txt): every Vietnamese
/// glyph, unlike egui's built-in font.
const FONT_REGULAR: &[u8] = include_bytes!("../../../assets/fonts/BeVietnamPro-Regular.ttf");
const FONT_BOLD: &[u8] = include_bytes!("../../../assets/fonts/BeVietnamPro-SemiBold.ttf");
const BOLD: &str = "bold";

pub fn bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(BOLD.into()))
}

pub fn regular(size: f32) -> FontId {
    FontId::proportional(size)
}

/// Fonts, text sizes (body 16 px, headings 24 px), roomier spacing and colors for both themes.
pub fn install(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::default();
    fonts.font_data.insert("bvp".into(), Arc::new(egui::FontData::from_static(FONT_REGULAR)));
    fonts.font_data.insert("bvp-bold".into(), Arc::new(egui::FontData::from_static(FONT_BOLD)));
    let prop = fonts.families.entry(FontFamily::Proportional).or_default();
    // egui's own fonts stay behind as fallback for symbols and emoji.
    prop.insert(0, "bvp".into());
    let mut bold_family = vec!["bvp-bold".to_string()];
    bold_family.extend(prop.iter().filter(|n| n.as_str() != "bvp").cloned());
    fonts.families.insert(FontFamily::Name(BOLD.into()), bold_family);
    fonts.families.entry(FontFamily::Monospace).or_default().push("bvp".into());
    ctx.set_fonts(fonts);

    ctx.all_styles_mut(|style| {
        use egui::TextStyle::*;
        style.text_styles = [
            (Small, regular(13.0)),
            (Body, regular(16.0)),
            (Button, regular(16.0)),
            (Heading, bold(24.0)),
            (Monospace, FontId::monospace(13.0)),
        ]
        .into();
        style.spacing.item_spacing = vec2(10.0, 10.0);
        style.spacing.button_padding = vec2(14.0, 7.0);
        style.spacing.interact_size.y = 34.0;
        // A scroll bar with its own column, never drawn over buttons.
        style.spacing.scroll = egui::style::ScrollStyle::solid();
        let w = &mut style.visuals.widgets;
        for v in [&mut w.inactive, &mut w.hovered, &mut w.active, &mut w.noninteractive, &mut w.open] {
            v.corner_radius = CornerRadius::same(8);
        }
    });
    // Light / dark follows the system; NANOCAST_THEME=light|dark forces one (screenshots, tests).
    match std::env::var("NANOCAST_THEME").as_deref() {
        Ok("dark") => ctx.set_theme(Theme::Dark),
        Ok("light") => ctx.set_theme(Theme::Light),
        _ => {}
    }
    for theme in [Theme::Light, Theme::Dark] {
        let p = Pal::of(theme == Theme::Dark);
        ctx.style_mut_of(theme, |style| {
            style.visuals.panel_fill = p.bg;
            style.visuals.window_fill = p.card;
            style.visuals.extreme_bg_color = p.input;
            style.visuals.selection.bg_fill = p.accent;
            style.visuals.hyperlink_color = p.accent;
            style.visuals.widgets.inactive.weak_bg_fill = p.button;
            style.visuals.widgets.inactive.bg_fill = p.button;
            style.visuals.widgets.hovered.weak_bg_fill = p.button_hover;
            style.visuals.widgets.hovered.bg_fill = p.button_hover;
            // Text in full contrast (egui's default is a softer gray); text fields get a border.
            style.visuals.override_text_color = Some(p.text);
            style.visuals.widgets.inactive.bg_stroke = egui::Stroke::new(1.0, p.border);
        });
    }
}

/// The colors of one theme.
#[derive(Clone, Copy)]
pub struct Pal {
    pub dark: bool,
    pub bg: Color32,
    pub card: Color32,
    pub card_hover: Color32,
    pub border: Color32,
    pub input: Color32,
    pub button: Color32,
    pub button_hover: Color32,
    pub text: Color32,
    pub weak: Color32,
    pub accent: Color32,
    pub accent_soft: Color32,
    pub green: Color32,
    pub green_soft: Color32,
    pub amber: Color32,
    pub amber_soft: Color32,
    pub red: Color32,
    pub red_soft: Color32,
}

impl Pal {
    pub fn of(dark: bool) -> Self {
        if dark {
            Self {
                dark,
                bg: Color32::from_rgb(0x12, 0x16, 0x1d),
                card: Color32::from_rgb(0x1d, 0x23, 0x2e),
                card_hover: Color32::from_rgb(0x26, 0x2e, 0x3b),
                border: Color32::from_rgb(0x33, 0x3c, 0x4b),
                input: Color32::from_rgb(0x0d, 0x10, 0x15),
                button: Color32::from_rgb(0x2a, 0x32, 0x40),
                button_hover: Color32::from_rgb(0x34, 0x3e, 0x4f),
                text: Color32::from_rgb(0xe9, 0xed, 0xf3),
                weak: Color32::from_rgb(0x9a, 0xa5, 0xb5),
                accent: Color32::from_rgb(0x4c, 0x8d, 0xff),
                accent_soft: Color32::from_rgb(0x1b, 0x2d, 0x4f),
                green: Color32::from_rgb(0x3d, 0xd6, 0x8c),
                green_soft: Color32::from_rgb(0x15, 0x36, 0x2a),
                amber: Color32::from_rgb(0xf5, 0xb3, 0x3d),
                amber_soft: Color32::from_rgb(0x3d, 0x2f, 0x14),
                red: Color32::from_rgb(0xff, 0x6b, 0x5e),
                red_soft: Color32::from_rgb(0x45, 0x1d, 0x1b),
            }
        } else {
            Self {
                dark,
                bg: Color32::from_rgb(0xf2, 0xf5, 0xf9),
                card: Color32::WHITE,
                card_hover: Color32::from_rgb(0xe8, 0xf0, 0xfe),
                border: Color32::from_rgb(0xd5, 0xdc, 0xe6),
                input: Color32::WHITE,
                button: Color32::from_rgb(0xe3, 0xe8, 0xef),
                button_hover: Color32::from_rgb(0xd3, 0xdb, 0xe6),
                text: Color32::from_rgb(0x14, 0x1a, 0x24),
                weak: Color32::from_rgb(0x5b, 0x66, 0x76),
                accent: Color32::from_rgb(0x1f, 0x5e, 0xdb),
                accent_soft: Color32::from_rgb(0xdd, 0xe8, 0xfd),
                green: Color32::from_rgb(0x12, 0x8a, 0x4b),
                green_soft: Color32::from_rgb(0xdb, 0xf3, 0xe5),
                amber: Color32::from_rgb(0xb4, 0x63, 0x00),
                amber_soft: Color32::from_rgb(0xfd, 0xee, 0xd2),
                red: Color32::from_rgb(0xc8, 0x2c, 0x22),
                red_soft: Color32::from_rgb(0xfb, 0xe1, 0xde),
            }
        }
    }
}

pub fn pal(ui: &Ui) -> Pal {
    Pal::of(ui.visuals().dark_mode)
}

/// What a colored element means.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tone {
    Accent,
    Good,
    Warn,
    Bad,
    Neutral,
}

impl Tone {
    /// (strong color, soft background)
    pub fn colors(self, p: &Pal) -> (Color32, Color32) {
        match self {
            Tone::Accent => (p.accent, p.accent_soft),
            Tone::Good => (p.green, p.green_soft),
            Tone::Warn => (p.amber, p.amber_soft),
            Tone::Bad => (p.red, p.red_soft),
            Tone::Neutral => (p.weak, p.button),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Icon {
    Tv,
    Screen,
    App,
    SoundOn,
    SoundOff,
    Pause,
    Play,
    Blank,
    Stop,
    Gear,
    Help,
    Back,
    Search,
    Lock,
    Warn,
    Check,
    Refresh,
    Wifi,
    Cast,
    Info,
    Chevron,
    Download,
}

/// Paints `icon` centered in `rect`.
pub fn draw_icon(p: &Painter, rect: Rect, icon: Icon, color: Color32) {
    let size = rect.width().min(rect.height());
    let u = size / 24.0;
    let c = rect.center();
    let pt = |x: f32, y: f32| pos2(c.x + (x - 12.0) * u, c.y + (y - 12.0) * u);
    let r = |x0: f32, y0: f32, x1: f32, y1: f32| Rect::from_min_max(pt(x0, y0), pt(x1, y1));
    let st = Stroke::new((2.0 * u).max(1.2), color);
    let line = |pts: &[(f32, f32)]| {
        p.add(Shape::line(pts.iter().map(|&(x, y)| pt(x, y)).collect(), st));
    };
    let arc = |cx: f32, cy: f32, rad: f32, a0: f32, a1: f32| {
        let n = 20;
        let pts = (0..=n)
            .map(|i| {
                let a = (a0 + (a1 - a0) * i as f32 / n as f32).to_radians();
                pt(cx + rad * a.cos(), cy + rad * a.sin())
            })
            .collect();
        p.add(Shape::line(pts, st));
    };
    let fill = |pts: &[(f32, f32)]| {
        p.add(Shape::convex_polygon(pts.iter().map(|&(x, y)| pt(x, y)).collect(), color, Stroke::NONE));
    };
    let cr = |v: f32| CornerRadius::same((v * u).round().clamp(0.0, 255.0) as u8);
    match icon {
        Icon::Tv => {
            p.rect_stroke(r(2.0, 5.0, 22.0, 18.0), cr(2.5), st, StrokeKind::Middle);
            line(&[(8.0, 1.5), (12.0, 5.0), (16.0, 1.5)]);
            line(&[(7.0, 21.5), (17.0, 21.5)]);
        }
        Icon::Screen => {
            p.rect_stroke(r(2.0, 3.0, 22.0, 16.0), cr(2.0), st, StrokeKind::Middle);
            line(&[(12.0, 16.0), (12.0, 20.5)]);
            line(&[(7.0, 20.5), (17.0, 20.5)]);
        }
        Icon::App => {
            p.rect_stroke(r(2.0, 4.0, 22.0, 20.0), cr(2.0), st, StrokeKind::Middle);
            line(&[(2.0, 8.5), (22.0, 8.5)]);
            for x in [5.0, 7.5, 10.0] {
                p.circle_filled(pt(x, 6.3), 0.9 * u, color);
            }
        }
        Icon::SoundOn | Icon::SoundOff => {
            fill(&[(2.5, 9.0), (7.0, 9.0), (12.0, 4.5), (12.0, 19.5), (7.0, 15.0), (2.5, 15.0)]);
            if icon == Icon::SoundOn {
                arc(12.0, 12.0, 4.5, -45.0, 45.0);
                arc(12.0, 12.0, 8.5, -50.0, 50.0);
            } else {
                line(&[(15.5, 9.0), (21.5, 15.0)]);
                line(&[(21.5, 9.0), (15.5, 15.0)]);
            }
        }
        Icon::Pause => {
            p.rect_filled(r(5.5, 4.5, 10.0, 19.5), cr(1.2), color);
            p.rect_filled(r(14.0, 4.5, 18.5, 19.5), cr(1.2), color);
        }
        Icon::Play => fill(&[(7.0, 4.0), (20.0, 12.0), (7.0, 20.0)]),
        Icon::Blank => {
            p.rect_stroke(r(2.0, 3.0, 22.0, 16.0), cr(2.0), st, StrokeKind::Middle);
            p.rect_filled(r(4.5, 5.5, 19.5, 13.5), cr(0.5), color);
            line(&[(7.0, 20.5), (17.0, 20.5)]);
        }
        Icon::Stop => {
            p.rect_filled(r(5.5, 5.5, 18.5, 18.5), cr(2.0), color);
        }
        Icon::Gear => {
            p.circle_stroke(pt(12.0, 12.0), 3.2 * u, st);
            p.circle_stroke(pt(12.0, 12.0), 7.0 * u, st);
            for i in 0..8 {
                let a = (i as f32 * 45.0).to_radians();
                let (s, co) = a.sin_cos();
                p.line_segment([pt(12.0 + 7.0 * co, 12.0 + 7.0 * s), pt(12.0 + 10.0 * co, 12.0 + 10.0 * s)], Stroke::new(3.0 * u, color));
            }
        }
        Icon::Help | Icon::Info => {
            p.circle_stroke(pt(12.0, 12.0), 10.0 * u, st);
            let glyph = if icon == Icon::Help { "?" } else { "i" };
            p.text(pt(12.0, 12.5), Align2::CENTER_CENTER, glyph, bold(14.0 * u), color);
        }
        Icon::Back => {
            line(&[(20.0, 12.0), (4.5, 12.0)]);
            line(&[(10.5, 6.0), (4.5, 12.0), (10.5, 18.0)]);
        }
        Icon::Chevron => line(&[(9.0, 5.0), (16.0, 12.0), (9.0, 19.0)]),
        Icon::Download => {
            line(&[(12.0, 3.0), (12.0, 15.0)]);
            line(&[(6.5, 9.5), (12.0, 15.0), (17.5, 9.5)]);
            line(&[(3.5, 16.0), (3.5, 20.5), (20.5, 20.5), (20.5, 16.0)]);
        }
        Icon::Search => {
            p.circle_stroke(pt(10.0, 10.0), 6.5 * u, st);
            line(&[(15.0, 15.0), (21.0, 21.0)]);
        }
        Icon::Lock => {
            p.rect_filled(r(5.0, 10.5, 19.0, 21.0), cr(2.0), color);
            arc(12.0, 10.5, 4.5, 180.0, 360.0);
        }
        Icon::Warn => {
            p.add(Shape::closed_line(vec![pt(12.0, 3.0), pt(22.0, 20.5), pt(2.0, 20.5)], st));
            line(&[(12.0, 9.0), (12.0, 14.5)]);
            p.circle_filled(pt(12.0, 17.5), 1.2 * u, color);
        }
        Icon::Check => line(&[(4.5, 12.5), (9.5, 17.5), (19.5, 6.5)]),
        Icon::Refresh => {
            arc(12.0, 12.0, 8.0, -60.0, 240.0);
            fill(&[(19.5, 2.5), (20.0, 9.5), (13.5, 7.5)]);
        }
        Icon::Wifi => {
            arc(12.0, 20.0, 5.0, 225.0, 315.0);
            arc(12.0, 20.0, 10.0, 225.0, 315.0);
            arc(12.0, 20.0, 15.0, 230.0, 310.0);
            p.circle_filled(pt(12.0, 20.0), 1.6 * u, color);
        }
        Icon::Cast => {
            line(&[(2.0, 8.0), (2.0, 5.0), (22.0, 5.0), (22.0, 19.0), (14.0, 19.0)]);
            arc(2.0, 19.0, 4.0, 270.0, 360.0);
            arc(2.0, 19.0, 8.0, 270.0, 360.0);
            p.circle_filled(pt(3.0, 18.0), 1.3 * u, color);
        }
    }
}

/// An icon as a widget.
pub fn icon(ui: &mut Ui, icon: Icon, size: f32, color: Color32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    draw_icon(ui.painter(), rect, icon, color);
    resp
}

/// Small round icon-only button (header).
pub fn icon_button(ui: &mut Ui, icon: Icon, tooltip: &str) -> Response {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::click());
    if resp.hovered() {
        ui.painter().circle_filled(rect.center(), 18.0, p.button_hover);
    }
    draw_icon(ui.painter(), rect.shrink(8.0), icon, p.text);
    resp.on_hover_text(tooltip).on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A flat text button with an icon in front ("← Back").
pub fn link_button(ui: &mut Ui, icon: Icon, text: &str) -> Response {
    let p = pal(ui);
    let galley = ui.painter().layout_no_wrap(text.to_string(), regular(16.0), p.accent);
    let size = vec2(galley.size().x + 34.0, 32.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.hovered() {
        ui.painter().rect_filled(rect, 8.0, p.accent_soft);
    }
    draw_icon(ui.painter(), Rect::from_center_size(pos2(rect.left() + 14.0, rect.center().y), Vec2::splat(16.0)), icon, p.accent);
    ui.painter().galley(pos2(rect.left() + 28.0, rect.center().y - galley.size().y / 2.0), galley, p.accent);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Button style for `big_button`.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Primary,
    Danger,
    Normal,
    /// A switch-like button that is on (Pause while paused).
    On,
}

/// A large rounded button. Taller than ~90 px: icon on top, label and `sub` centered below
/// (a tile); otherwise icon and label side by side.
pub fn big_button(ui: &mut Ui, icon: Icon, label: &str, sub: Option<&str>, kind: Kind, size: Vec2, enabled: bool) -> Response {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    let hovered = enabled && resp.hovered();
    let (mut fill, mut fg, mut stroke) = match kind {
        Kind::Primary => (if hovered { p.accent.gamma_multiply(0.88) } else { p.accent }, Color32::WHITE, Stroke::NONE),
        Kind::Danger => {
            // White on the dark theme's light red is too soft: use a deeper red there.
            let red = if p.dark { Color32::from_rgb(0xc9, 0x3a, 0x2f) } else { p.red };
            (if hovered { red.gamma_multiply(0.88) } else { red }, Color32::WHITE, Stroke::NONE)
        }
        Kind::Normal => (if hovered { p.card_hover } else { p.card }, p.text, Stroke::new(1.0, if hovered { p.accent } else { p.border })),
        Kind::On => (p.amber_soft, p.amber, Stroke::new(2.0, p.amber)),
    };
    if !enabled {
        fill = p.button;
        fg = p.weak;
        stroke = Stroke::NONE;
    }
    let painter = ui.painter();
    painter.rect(rect, 14.0, fill, stroke, StrokeKind::Inside);
    let icon_color = if kind == Kind::Normal && enabled { p.accent } else { fg };
    if size.y >= 90.0 {
        let icon_size = if sub.is_some() { (size.y * 0.22).min(40.0) } else { (size.y * 0.32).min(44.0) };
        let mut y = rect.top() + 12.0;
        let label_g = painter.layout(label.to_string(), bold(18.0), fg, rect.width() - 16.0);
        let sub_g = sub.map(|s| {
            painter.layout(s.to_string(), regular(13.0), if enabled && kind == Kind::Normal { p.weak } else { fg }, rect.width() - 20.0)
        });
        let total = icon_size + 8.0 + label_g.size().y + sub_g.as_ref().map_or(0.0, |g| g.size().y + 2.0);
        y += ((rect.height() - 24.0) - total).max(0.0) / 2.0;
        draw_icon(painter, Rect::from_center_size(pos2(rect.center().x, y + icon_size / 2.0), Vec2::splat(icon_size)), icon, icon_color);
        y += icon_size + 8.0;
        painter.galley(pos2(rect.center().x - label_g.size().x / 2.0, y), label_g.clone(), fg);
        y += label_g.size().y + 2.0;
        if let Some(g) = sub_g {
            painter.galley(pos2(rect.center().x - g.size().x / 2.0, y), g, fg);
        }
    } else {
        let icon_size = 22.0;
        let galley = painter.layout_no_wrap(label.to_string(), bold(17.0), fg);
        let w = icon_size + 10.0 + galley.size().x;
        let x = rect.center().x - w / 2.0;
        draw_icon(painter, Rect::from_center_size(pos2(x + icon_size / 2.0, rect.center().y), Vec2::splat(icon_size)), icon, icon_color);
        painter.galley(pos2(x + icon_size + 10.0, rect.center().y - galley.size().y / 2.0), galley, fg);
    }
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// One TV in the list of step 1.
pub struct TvCard<'a> {
    pub name: &'a str,
    pub badge: &'a str,
    pub badge_tone: Tone,
    pub status: &'a str,
    pub locked: bool,
    pub chromecast: bool,
    /// A small button inside the card ("Install app on TV"), with its tooltip.
    pub action: Option<(&'a str, &'a str)>,
}

/// The card's response, and whether its inner `action` button was clicked instead.
pub fn tv_card(ui: &mut Ui, card: &TvCard) -> (Response, bool) {
    let p = pal(ui);
    let size = vec2(ui.available_width(), 76.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    // Added after the card, so it wins the hover / click where they overlap.
    let action = card.action.map(|(label, tip)| {
        let galley = ui.painter().layout_no_wrap(label.to_string(), bold(13.0), p.accent);
        let pill = Rect::from_min_size(pos2(rect.right() - 44.0 - galley.size().x - 34.0, rect.center().y - 15.0), vec2(galley.size().x + 34.0, 30.0));
        let r = ui.interact(pill, resp.id.with("action"), Sense::click()).on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand);
        (galley, pill, r)
    });
    let hovered = resp.hovered() && !action.as_ref().is_some_and(|a| a.2.hovered());
    let painter = ui.painter();
    painter.rect(
        rect,
        14.0,
        if hovered { p.card_hover } else { p.card },
        Stroke::new(if hovered { 2.0 } else { 1.0 }, if hovered { p.accent } else { p.border }),
        StrokeKind::Inside,
    );
    let (tone, soft) = card.badge_tone.colors(&p);
    let ic = Rect::from_center_size(pos2(rect.left() + 40.0, rect.center().y), Vec2::splat(48.0));
    painter.circle_filled(ic.center(), 24.0, soft);
    draw_icon(painter, ic.shrink(12.0), if card.chromecast { Icon::Cast } else { Icon::Tv }, tone);
    let x = rect.left() + 78.0;
    let right = action.as_ref().map_or(rect.right() - 40.0, |a| a.1.left() - 8.0);
    let text_w = right - x;
    let name = painter.layout_no_wrap(card.name.to_string(), bold(18.0), p.text);
    let name_clip = Rect::from_min_size(pos2(x, rect.top() + 12.0), vec2(text_w, name.size().y));
    painter.with_clip_rect(name_clip).galley(name_clip.min, name, p.text);
    let mut bx = x;
    let by = rect.top() + 44.0;
    let badge = painter.layout_no_wrap(card.badge.to_string(), bold(13.0), tone);
    let brect = Rect::from_min_size(pos2(bx, by), badge.size() + vec2(16.0, 6.0));
    painter.rect_filled(brect, 10.0, soft);
    painter.galley(brect.min + vec2(8.0, 3.0), badge, tone);
    bx = brect.right() + 10.0;
    if card.locked {
        draw_icon(painter, Rect::from_min_size(pos2(bx, by + 2.0), Vec2::splat(16.0)), Icon::Lock, p.weak);
        bx += 20.0;
    }
    let status = painter.layout_no_wrap(card.status.to_string(), regular(14.0), p.weak);
    let srect = Rect::from_min_max(pos2(bx, by + 2.0), pos2(right.max(bx), by + 24.0));
    painter.with_clip_rect(srect).galley(srect.min, status, p.weak);
    let mut clicked = false;
    if let Some((galley, pill, r)) = action {
        let on = r.hovered();
        painter.rect(pill, 15.0, if on { p.accent_soft } else { p.card }, Stroke::new(1.0, if on { p.accent } else { p.border }), StrokeKind::Inside);
        draw_icon(painter, Rect::from_center_size(pos2(pill.left() + 16.0, pill.center().y), Vec2::splat(15.0)), Icon::Download, p.accent);
        painter.galley(pos2(pill.left() + 28.0, pill.center().y - galley.size().y / 2.0), galley, p.accent);
        clicked = r.clicked();
    }
    draw_icon(
        painter,
        Rect::from_center_size(pos2(rect.right() - 22.0, rect.center().y), Vec2::splat(20.0)),
        Icon::Chevron,
        if hovered { p.accent } else { p.weak },
    );
    (resp.on_hover_cursor(egui::CursorIcon::PointingHand), clicked)
}

/// A slim clickable row ("My TV is not listed?").
pub fn row_button(ui: &mut Ui, icon: Icon, text: &str, open: bool) -> Response {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 50.0), Sense::click());
    let hovered = resp.hovered();
    let painter = ui.painter();
    painter.rect(rect, 12.0, if hovered { p.card_hover } else { Color32::TRANSPARENT }, Stroke::new(1.0, p.border), StrokeKind::Inside);
    draw_icon(painter, Rect::from_center_size(pos2(rect.left() + 26.0, rect.center().y), Vec2::splat(22.0)), icon, p.accent);
    painter.text(pos2(rect.left() + 48.0, rect.center().y), Align2::LEFT_CENTER, text, bold(16.0), p.accent);
    let chev = Rect::from_center_size(pos2(rect.right() - 22.0, rect.center().y), Vec2::splat(18.0));
    if open {
        // Chevron pointing down.
        let c = chev.center();
        painter.add(Shape::line(vec![c + vec2(-6.0, -3.0), c + vec2(0.0, 4.0), c + vec2(6.0, -3.0)], Stroke::new(2.0, p.weak)));
    } else {
        draw_icon(painter, chev, Icon::Chevron, p.weak);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// An application window in the "one app" list.
pub fn app_row(ui: &mut Ui, app: &str, title: &str, note: Option<&str>) -> Response {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(vec2(ui.available_width(), 56.0), Sense::click());
    let hovered = resp.hovered();
    let painter = ui.painter();
    painter.rect(
        rect,
        12.0,
        if hovered { p.card_hover } else { p.card },
        Stroke::new(1.0, if hovered { p.accent } else { p.border }),
        StrokeKind::Inside,
    );
    let center = pos2(rect.left() + 30.0, rect.center().y);
    painter.circle_filled(center, 18.0, avatar_color(app, p.dark));
    let letter: String = app.chars().find(|c| c.is_alphanumeric()).map(|c| c.to_uppercase().collect()).unwrap_or_else(|| "?".into());
    painter.text(center, Align2::CENTER_CENTER, letter, bold(17.0), Color32::WHITE);
    let x = rect.left() + 60.0;
    let clip = Rect::from_min_max(pos2(x, rect.top()), pos2(rect.right() - 12.0, rect.bottom()));
    let painter = painter.with_clip_rect(clip);
    painter.text(pos2(x, rect.top() + 18.0), Align2::LEFT_CENTER, app, bold(16.0), p.text);
    let line2 = match note {
        Some(n) => format!("{title}  ·  {n}"),
        None => title.to_string(),
    };
    painter.text(pos2(x, rect.top() + 39.0), Align2::LEFT_CENTER, line2, regular(13.0), p.weak);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

fn avatar_color(name: &str, dark: bool) -> Color32 {
    const COLORS: [(u8, u8, u8); 8] = [
        (0xd9, 0x48, 0x2b),
        (0x1f, 0x7a, 0xd6),
        (0x1e, 0x8e, 0x5a),
        (0x8e, 0x44, 0xc9),
        (0xc2, 0x7c, 0x0e),
        (0x0e, 0x8a, 0x9a),
        (0xc2, 0x35, 0x7a),
        (0x55, 0x65, 0x7a),
    ];
    let h = name.to_lowercase().bytes().fold(5381u32, |h, b| h.wrapping_mul(33) ^ b as u32);
    let (r, g, b) = COLORS[(h % COLORS.len() as u32) as usize];
    let c = Color32::from_rgb(r, g, b);
    if dark {
        c.gamma_multiply(0.9)
    } else {
        c
    }
}

/// A big on/off switch. Returns a response that is `changed()` when flipped.
pub fn switch(ui: &mut Ui, on: &mut bool, enabled: bool) -> Response {
    let p = pal(ui);
    let size = vec2(58.0, 32.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    if enabled && resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let t = ui.ctx().animate_bool_responsive(resp.id, *on);
    let track = if !enabled {
        p.button
    } else if *on {
        p.green
    } else {
        p.border
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 16.0, track);
    let x = egui::lerp((rect.left() + 16.0)..=(rect.right() - 16.0), t);
    painter.circle_filled(pos2(x, rect.center().y), 12.5, if enabled { Color32::WHITE } else { p.weak });
    if enabled {
        resp.on_hover_cursor(egui::CursorIcon::PointingHand)
    } else {
        resp
    }
}

/// A colored box with an icon, a bold title and optional lines below (status or a problem
/// with the steps to fix it).
pub fn banner(ui: &mut Ui, tone: Tone, icon_kind: Icon, title: &str, lines: &[&str]) -> Response {
    let p = pal(ui);
    let (strong, soft) = tone.colors(&p);
    egui::Frame::new()
        .fill(soft)
        .corner_radius(14.0)
        .inner_margin(egui::Margin::symmetric(14, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                icon(ui, icon_kind, 26.0, strong);
                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 4.0;
                    ui.add(egui::Label::new(RichText::new(title).font(bold(17.0)).color(strong)).wrap());
                    for l in lines {
                        ui.add(egui::Label::new(RichText::new(*l).color(p.text)).wrap());
                    }
                });
            });
        })
        .response
}

/// "Searching" animation: a TV with waves spreading out. Keeps the UI repainting.
pub fn radar(ui: &mut Ui, size: f32) -> Response {
    let p = pal(ui);
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::hover());
    let time = ui.input(|i| i.time) as f32;
    let c = rect.center();
    let max_r = size / 2.0;
    for k in 0..3 {
        let phase = (time / 2.0 + k as f32 / 3.0).fract();
        let alpha = (1.0 - phase).powf(1.5);
        let r = max_r * (0.35 + 0.65 * phase);
        ui.painter().circle_stroke(c, r, Stroke::new(2.0, p.accent.gamma_multiply(alpha)));
    }
    ui.painter().circle_filled(c, max_r * 0.36, p.accent_soft);
    draw_icon(ui.painter(), Rect::from_center_size(c, Vec2::splat(max_r * 0.4)), Icon::Tv, p.accent);
    ui.ctx().request_repaint();
    resp
}

/// 1 · 2 · 3 step indicator; `current` is zero-based.
pub fn steps(ui: &mut Ui, current: usize, labels: [&str; 3]) {
    let p = pal(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 34.0), Sense::hover());
    let painter = ui.painter();
    let w = rect.width() / 3.0;
    for (i, label) in labels.iter().enumerate() {
        let cell = Rect::from_min_size(pos2(rect.left() + w * i as f32, rect.top()), vec2(w, rect.height()));
        let done = i < current;
        let now = i == current;
        let color = if now || done { p.accent } else { p.weak };
        let galley =
            painter.layout_no_wrap(label.to_string(), if now { bold(15.0) } else { regular(15.0) }, if now { p.text } else { p.weak });
        let total = 26.0 + 8.0 + galley.size().x;
        let x = cell.center().x - total.min(w - 8.0) / 2.0;
        let cc = pos2(x + 13.0, cell.center().y);
        if now || done {
            painter.circle_filled(cc, 13.0, color);
        } else {
            painter.circle_stroke(cc, 12.0, Stroke::new(1.5, color));
        }
        if done {
            draw_icon(painter, Rect::from_center_size(cc, Vec2::splat(16.0)), Icon::Check, Color32::WHITE);
        } else {
            painter.text(cc, Align2::CENTER_CENTER, (i + 1).to_string(), bold(14.0), if now { Color32::WHITE } else { color });
        }
        let clip = Rect::from_min_max(pos2(x + 34.0, cell.top()), pos2(cell.right() - 4.0, cell.bottom()));
        painter.with_clip_rect(clip).galley(pos2(x + 34.0, cell.center().y - galley.size().y / 2.0), galley, p.text);
        if i < 2 {
            painter.line_segment(
                [pos2(cell.right() - 6.0, cell.center().y), pos2(cell.right() + 6.0, cell.center().y)],
                Stroke::new(1.5, p.border),
            );
        }
    }
}

/// Simple drawings for the first-run guide (page 0, 1, 2).
pub fn illustration(ui: &mut Ui, page: usize, size: Vec2) {
    let p = pal(ui);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    let painter = ui.painter();
    painter.rect_filled(rect, 18.0, p.accent_soft);
    let c = rect.center();
    let st = |w: f32, col: Color32| Stroke::new(w, col);
    match page {
        0 => {
            // A TV showing the NANOCast app, Wi-Fi above.
            let tv = Rect::from_center_size(c + vec2(0.0, 10.0), vec2(200.0, 112.0));
            painter.rect(tv, 10.0, p.card, st(5.0, p.text), StrokeKind::Middle);
            painter.line_segment([pos2(c.x - 34.0, tv.bottom() + 14.0), pos2(c.x + 34.0, tv.bottom() + 14.0)], st(5.0, p.text));
            painter.line_segment([pos2(c.x, tv.bottom()), pos2(c.x, tv.bottom() + 14.0)], st(5.0, p.text));
            let tile = Rect::from_center_size(tv.center() - vec2(0.0, 8.0), vec2(56.0, 56.0));
            painter.rect_filled(tile, 14.0, Color32::from_rgb(0x12, 0x3c, 0x7a));
            draw_icon(painter, tile.shrink(12.0), Icon::Cast, Color32::from_rgb(0x34, 0xd3, 0x99));
            painter.text(pos2(tv.center().x, tile.bottom() + 12.0), Align2::CENTER_CENTER, "NANOCast", bold(13.0), p.text);
            draw_icon(painter, Rect::from_center_size(pos2(tv.right() + 34.0, tv.top() - 6.0), Vec2::splat(44.0)), Icon::Wifi, p.accent);
            draw_icon(painter, Rect::from_center_size(pos2(tv.left() - 34.0, tv.top() - 6.0), Vec2::splat(44.0)), Icon::Wifi, p.accent);
        }
        1 => {
            // A laptop, an arrow, a TV.
            let lap = Rect::from_center_size(c + vec2(-92.0, -4.0), vec2(120.0, 76.0));
            painter.rect(lap, 6.0, p.card, st(4.0, p.text), StrokeKind::Middle);
            painter.add(Shape::convex_polygon(
                vec![
                    pos2(lap.left() - 14.0, lap.bottom() + 14.0),
                    pos2(lap.right() + 14.0, lap.bottom() + 14.0),
                    pos2(lap.right(), lap.bottom() + 2.0),
                    pos2(lap.left(), lap.bottom() + 2.0),
                ],
                p.text,
                Stroke::NONE,
            ));
            for (i, sel) in [(0, true), (1, false)] {
                let row = Rect::from_min_size(lap.min + vec2(10.0, 12.0 + i as f32 * 26.0), vec2(100.0, 20.0));
                painter.rect_filled(row, 5.0, if sel { p.accent } else { p.button });
            }
            painter.arrow(c + vec2(-22.0, 0.0), vec2(46.0, 0.0), st(4.0, p.accent));
            let tv = Rect::from_center_size(c + vec2(100.0, -6.0), vec2(120.0, 76.0));
            painter.rect(tv, 8.0, p.card, st(4.0, p.text), StrokeKind::Middle);
            painter.rect_filled(tv.shrink(10.0), 4.0, p.green.gamma_multiply(0.6));
            painter.line_segment(
                [pos2(tv.center().x - 22.0, tv.bottom() + 12.0), pos2(tv.center().x + 22.0, tv.bottom() + 12.0)],
                st(4.0, p.text),
            );
        }
        _ => {
            // The three buttons of step 3.
            let items = [(Icon::Pause, p.amber), (Icon::Blank, p.weak), (Icon::Stop, p.red)];
            for (i, (ic, col)) in items.into_iter().enumerate() {
                let b = Rect::from_center_size(c + vec2((i as f32 - 1.0) * 96.0, 0.0), vec2(80.0, 80.0));
                painter.rect(b, 16.0, p.card, st(3.0, col), StrokeKind::Inside);
                draw_icon(painter, b.shrink(22.0), ic, col);
            }
        }
    }
}

/// Dots under the guide pages.
pub fn page_dots(ui: &mut Ui, n: usize, current: usize) {
    let p = pal(ui);
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), 14.0), Sense::hover());
    let start = rect.center().x - (n as f32 - 1.0) * 10.0;
    for i in 0..n {
        let c = pos2(start + i as f32 * 20.0, rect.center().y);
        if i == current {
            ui.painter().circle_filled(c, 5.0, p.accent);
        } else {
            ui.painter().circle_filled(c, 4.0, p.border);
        }
    }
}
