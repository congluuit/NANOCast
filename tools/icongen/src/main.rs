//! Renders the hand-drawn SVG assets into every raster form the apps need:
//!   assets/icon.svg   -> assets/generated/icon-<n>.png (16..1024, 1024 for the macOS .icns),
//!                        assets/nanocast.ico,
//!                        receiver-androidtv/.../mipmap-*/ic_launcher.png
//!   assets/banner.svg -> assets/generated/banner-640x360.png,
//!                        receiver-androidtv/.../drawable-xhdpi/banner.png
//! Run from anywhere: `cargo run -p icongen`.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use resvg::{tiny_skia, usvg};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").canonicalize().expect("repo root")
}

fn load_svg(path: &Path) -> Result<usvg::Tree> {
    let data = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let mut opt = usvg::Options::default();
    opt.fontdb_mut().load_system_fonts();
    usvg::Tree::from_data(&data, &opt).with_context(|| format!("parsing {}", path.display()))
}

fn render(tree: &usvg::Tree, w: u32, h: u32) -> tiny_skia::Pixmap {
    let mut pixmap = tiny_skia::Pixmap::new(w, h).expect("pixmap");
    let sx = w as f32 / tree.size().width();
    let sy = h as f32 / tree.size().height();
    resvg::render(tree, tiny_skia::Transform::from_scale(sx, sy), &mut pixmap.as_mut());
    pixmap
}

fn write_png(pixmap: &tiny_skia::Pixmap, path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(path, pixmap.encode_png()?).with_context(|| format!("writing {}", path.display()))?;
    println!("  {}", path.strip_prefix(repo_root()).unwrap_or(path).display());
    Ok(())
}

fn main() -> Result<()> {
    let root = repo_root();
    let gen = root.join("assets/generated");
    let android_res = root.join("receiver-androidtv/app/src/main/res");

    println!("icon.svg:");
    let icon = load_svg(&root.join("assets/icon.svg"))?;
    for size in [16u32, 24, 32, 48, 64, 128, 256, 512, 1024] {
        write_png(&render(&icon, size, size), &gen.join(format!("icon-{size}.png")))?;
    }

    // Windows .ico (multi-size). ICO stores straight alpha, tiny-skia premultiplied.
    let mut dir = ico::IconDir::new(ico::ResourceType::Icon);
    for size in [16u32, 24, 32, 48, 64, 128, 256] {
        let rgba = render(&icon, size, size).take_demultiplied();
        let image = ico::IconImage::from_rgba_data(size, size, rgba);
        dir.add_entry(ico::IconDirEntry::encode(&image)?);
    }
    let ico_path = root.join("assets/nanocast.ico");
    dir.write(fs::File::create(&ico_path)?)?;
    println!("  assets/nanocast.ico ({} sizes)", dir.entries().len());

    // Android launcher icons (48 dp at each density).
    for (density, px) in [("mdpi", 48u32), ("hdpi", 72), ("xhdpi", 96), ("xxhdpi", 144), ("xxxhdpi", 192)] {
        write_png(&render(&icon, px, px), &android_res.join(format!("mipmap-{density}/ic_launcher.png")))?;
    }

    println!("banner.svg:");
    let banner = load_svg(&root.join("assets/banner.svg"))?;
    let banner_px = render(&banner, 640, 360);
    write_png(&banner_px, &gen.join("banner-640x360.png"))?;
    // Android TV banner: 320x180 dp, shipped at xhdpi (640x360 px).
    write_png(&banner_px, &android_res.join("drawable-xhdpi/banner.png"))?;

    Ok(())
}
