//! Phase 0 spike, item 6: compare two screenshots of the same fixture with a perceptual metric rather than
//! exact pixels (text rasterization and antialiasing never match exactly between renderers).
//!
//! ```text
//! parity-compare a.png b.png diff.png [--min 0.95]
//! ```
//! Prints SSIM (1.0 = identical), writes a heat-map of where they differ, and exits 1 below `--min`.

use image_compare::Algorithm;

fn main() {
    let mut args: Vec<String> = std::env::args().skip(1).collect();
    let min = match args.iter().position(|a| a == "--min") {
        Some(i) => {
            let v = args.remove(i + 1).parse::<f64>().expect("--min needs a number");
            args.remove(i);
            Some(v)
        }
        None => None,
    };
    let [a, b, diff] = <[String; 3]>::try_from(args).expect("usage: a.png b.png diff.png [--min S]");
    let (ia, ib) = (image::open(&a).expect("open a").to_rgb8(), image::open(&b).expect("open b").to_rgb8());
    if ia.dimensions() != ib.dimensions() {
        eprintln!("size mismatch: {:?} vs {:?}", ia.dimensions(), ib.dimensions());
        std::process::exit(2);
    }
    let gray = image_compare::gray_similarity_structure(&Algorithm::MSSIMSimple, &image::DynamicImage::ImageRgb8(ia.clone()).to_luma8(), &image::DynamicImage::ImageRgb8(ib.clone()).to_luma8()).expect("gray ssim");
    let rgb = image_compare::rgb_hybrid_compare(&ia, &ib).expect("rgb hybrid");
    println!("SSIM (gray, MSSIM): {:.4}", gray.score);
    println!("hybrid RGB score:   {:.4}", rgb.score);
    rgb.image.to_color_map().save(&diff).expect("save diff");
    println!("wrote {diff}");
    if let Some(min) = min {
        if gray.score < min {
            eprintln!("FAIL: {:.4} < {min}", gray.score);
            std::process::exit(1);
        }
    }
}
