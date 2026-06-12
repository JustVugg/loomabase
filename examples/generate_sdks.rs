use std::path::PathBuf;

use loomabase::codegen::{SdkLanguage, generate_sdk};
use loomabase::schema::{Contract, todos_table};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let output = std::env::var("LOOMABASE_SDK_OUTPUT")
        .map_or_else(|_| PathBuf::from("target/generated-sdks"), PathBuf::from);
    let contract = Contract::new(vec![todos_table()])?;
    for language in [
        SdkLanguage::Swift,
        SdkLanguage::Kotlin,
        SdkLanguage::TypeScript,
        SdkLanguage::Dart,
    ] {
        let directory = output.join(format!("{language:?}").to_lowercase());
        std::fs::create_dir_all(&directory)?;
        for (name, contents) in generate_sdk(&contract, language).files {
            std::fs::write(directory.join(name), contents)?;
        }
    }
    println!("Generated typed SDK contracts under {}", output.display());
    Ok(())
}
