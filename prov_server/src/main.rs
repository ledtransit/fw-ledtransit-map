use minify_html::{Cfg, minify};

fn main() {
    // 1. Copy files to output dir
    let files_to_copy = ["favicon.ico", "background.webp", "setup-wifi.html", "styles.css"];

    // 2. Translate files in output dir
    let files_to_translate = [
        "setup-wifi.html",
    ];
    let language_files = [
        "lang/en.json",
        "lang/de.json",
    ];

    // 3. Minify files in output dir
    let files_to_minify = [
        "setup-wifi+en.html",
        "setup-wifi+de.html",
        "styles.css",
    ];

    let input_dir = "public";
    let output_dir = "../assets/prov_public";

    // Clean output directory
    std::fs::create_dir_all(output_dir).unwrap_or_else(|_| panic!("Failed to create output directory: {}", output_dir));
    for entry in std::fs::read_dir(output_dir).unwrap_or_else(|_| panic!("Failed to read output directory: {}", output_dir)) {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_file() {
            std::fs::remove_file(&path).unwrap_or_else(|_| panic!("Failed to remove file: {:?}", path));
        }
    }

    // Copy files from input to output directory
    for file_path in files_to_copy {
        let input_path = format!("{}/{}", input_dir, file_path);
        let output_path = format!("{}/{}", output_dir, file_path);

        std::fs::copy(&input_path, &output_path)
            .unwrap_or_else(|_| panic!("Failed to copy file: {} to {}", input_path, output_path));

        println!("Copied {} -> {}", input_path, output_path);
    }

    // Translate files in output directory
    for file_path in files_to_translate {
        let input_path = format!("{}/{}", output_dir, file_path);
        let output_path = format!("{}/{}", output_dir, file_path);

        // Read the input file
        let input_content = std::fs::read_to_string(&input_path)
            .unwrap_or_else(|_| panic!("Failed to read file: {}", input_path));

        for language_file in &language_files {
            let language_content = std::fs::read_to_string(format!("{}/{}", input_dir, language_file))
                .unwrap_or_else(|_| panic!("Failed to read language file: {}", language_file));
            let translations: serde_json::Value = serde_json::from_str(&language_content)
                .unwrap_or_else(|_| panic!("Failed to parse language JSON: {}", language_file));

            // Replace translation keys in the input content with the corresponding translations
            let mut translated_content = input_content.clone();
            let re = regex::Regex::new(r"\{\{(.*?)\}\}").unwrap();
            for cap in re.captures_iter(&input_content) {
                let key = &cap[1];
                if let Some(value) = translations.get(key) {
                    translated_content = translated_content.replace(&cap[0], value.as_str().unwrap_or(""));
                } else {
                    println!("Warning: No translation found for key '{}' in file '{}'", key, language_file);
                }
            }

            // Write the translated content to the output file with locale suffix
            let locale_suffix = language_file.split('/').last().unwrap().split('.').next().unwrap();
            let output_path_with_locale = output_path.replace(".html", &format!("+{}.html", locale_suffix));
            std::fs::write(&output_path_with_locale, translated_content)
                .unwrap_or_else(|_| panic!("Failed to write file: {}", output_path_with_locale));

            println!("Translated {} -> {}", input_path, output_path_with_locale);
        }

        // Remove the original untranslated file from output directoy
        std::fs::remove_file(&input_path)
            .unwrap_or_else(|_| panic!("Failed to remove untranslated file: {}", input_path));
    }

    // Minify files in output directory
    for file_path in files_to_minify {
        let input_path = format!("{}/{}", output_dir, file_path);
        let output_path = format!("{}/{}", output_dir, file_path);

        let input_content = std::fs::read_to_string(&input_path)
            .unwrap_or_else(|_| panic!("Failed to read file: {}", input_path));

        // Minify HTML/JS/CSS content
        let cfg = Cfg { minify_js: true, ..Default::default() };
        let minified_content = minify(input_content.as_bytes(), &cfg);

        // Write the minified content to the output file
        std::fs::write(&output_path, minified_content)
            .unwrap_or_else(|_| panic!("Failed to write file: {}", output_path));

        println!("Minified {} -> {}", input_path, output_path);
    }
}
