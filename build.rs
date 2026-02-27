fn main() {
    println!("cargo:rerun-if-changed=assets/icon.png");

    // Solo necesario en Windows
    if std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default() != "windows" {
        return;
    }

    let png_path = "assets/icon.png";
    let ico_path = "assets/icon.ico";

    if !std::path::Path::new(png_path).exists() {
        println!("cargo:warning=Ponés tu logo en assets/icon.png para que aparezca en el .exe");
        return;
    }

    match png_to_ico(png_path, ico_path) {
        Ok(_) => {
            let mut res = winres::WindowsResource::new();
            res.set_icon(ico_path);
            if let Err(e) = res.compile() {
                println!("cargo:warning=No se pudo embeber el ícono: {}", e);
            }
        }
        Err(e) => {
            println!("cargo:warning=No se pudo convertir el PNG a ICO: {}", e);
        }
    }
}

/// Convierte un PNG a ICO (16x16, 32x32, 48x48) y lo escribe en disco
fn png_to_ico(png_path: &str, ico_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    use image::imageops::FilterType;

    let img = image::open(png_path)?;
    let sizes: &[u32] = &[16, 32, 48];

    // Generar datos BMP para cada tamaño
    let mut bmp_chunks: Vec<Vec<u8>> = Vec::new();
    for &size in sizes {
        let resized = img.resize_exact(size, size, FilterType::Lanczos3);
        let rgba = resized.to_rgba8();

        let and_row_stride = ((size + 31) / 32 * 4) as usize;
        let and_mask = vec![0u8; and_row_stride * size as usize];

        let mut bmp = Vec::new();

        // BITMAPINFOHEADER (40 bytes)
        bmp.extend_from_slice(&40u32.to_le_bytes());
        bmp.extend_from_slice(&(size as i32).to_le_bytes());
        bmp.extend_from_slice(&(size as i32 * 2).to_le_bytes()); // altura doble = ICO format
        bmp.extend_from_slice(&1u16.to_le_bytes());   // planes
        bmp.extend_from_slice(&32u16.to_le_bytes());  // bpp
        bmp.extend_from_slice(&0u32.to_le_bytes());   // BI_RGB
        bmp.extend_from_slice(&(size * size * 4).to_le_bytes()); // sizeImage
        bmp.extend_from_slice(&0i32.to_le_bytes());
        bmp.extend_from_slice(&0i32.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());
        bmp.extend_from_slice(&0u32.to_le_bytes());

        // Píxeles: RGBA → BGRA, orden bottom-up
        for row in (0..size).rev() {
            for col in 0..size {
                let p = rgba.get_pixel(col, row);
                bmp.push(p[2]); // B
                bmp.push(p[1]); // G
                bmp.push(p[0]); // R
                bmp.push(p[3]); // A
            }
        }

        // AND mask (todo ceros = completamente opaco)
        bmp.extend_from_slice(&and_mask);
        bmp_chunks.push(bmp);
    }

    // Construir el archivo ICO
    let count = sizes.len() as u16;
    let mut ico = Vec::new();

    // ICONDIR
    ico.extend_from_slice(&0u16.to_le_bytes()); // reserved
    ico.extend_from_slice(&1u16.to_le_bytes()); // type = icon
    ico.extend_from_slice(&count.to_le_bytes());

    // Calcular offset del primer chunk de imagen
    let dir_size = 6u32 + 16 * count as u32;
    let mut offset = dir_size;

    // ICONDIRENTRY para cada tamaño
    for (i, &size) in sizes.iter().enumerate() {
        let chunk_size = bmp_chunks[i].len() as u32;
        ico.push(if size >= 256 { 0 } else { size as u8 }); // width
        ico.push(if size >= 256 { 0 } else { size as u8 }); // height
        ico.push(0); // color count
        ico.push(0); // reserved
        ico.extend_from_slice(&1u16.to_le_bytes());  // planes
        ico.extend_from_slice(&32u16.to_le_bytes()); // bit count
        ico.extend_from_slice(&chunk_size.to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += chunk_size;
    }

    // Datos de imagen
    for chunk in &bmp_chunks {
        ico.extend_from_slice(chunk);
    }

    std::fs::write(ico_path, &ico)?;
    Ok(())
}
