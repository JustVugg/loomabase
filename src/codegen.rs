//! Dependency-free typed SDK model generation from a Loomabase sync contract.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use crate::schema::{ColumnType, Contract, LIVENESS_COLUMN};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub enum SdkLanguage {
    Swift,
    Kotlin,
    TypeScript,
    Dart,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GeneratedSdk {
    pub language: SdkLanguage,
    pub files: BTreeMap<String, String>,
}

#[must_use]
pub fn generate_sdk(contract: &Contract, language: SdkLanguage) -> GeneratedSdk {
    let mut files = BTreeMap::new();
    for table in contract.tables() {
        let type_name = pascal_case(table.name());
        let mut output = match language {
            SdkLanguage::Swift => format!(
                "import Foundation\n\npublic struct {type_name}: Codable, Identifiable, Sendable {{\n    public var id: String\n"
            ),
            SdkLanguage::Kotlin => format!(
                "package dev.loomabase.generated\n\nimport kotlinx.serialization.Serializable\n\n@Serializable\ndata class {type_name}(\n    val id: String,\n"
            ),
            SdkLanguage::TypeScript => format!("export interface {type_name} {{\n  id: string;\n"),
            SdkLanguage::Dart => format!("class {type_name} {{\n  final String id;\n"),
        };
        for column in table.columns() {
            match language {
                SdkLanguage::Swift => {
                    let _ = writeln!(
                        output,
                        "    public var {}: {}",
                        column.name,
                        swift_type(column.ty)
                    );
                }
                SdkLanguage::Kotlin => {
                    let _ = writeln!(
                        output,
                        "    val {}: {},",
                        column.name,
                        kotlin_type(column.ty)
                    );
                }
                SdkLanguage::TypeScript => {
                    let _ = writeln!(output, "  {}: {};", column.name, typescript_type(column.ty));
                }
                SdkLanguage::Dart => {
                    let _ = writeln!(output, "  final {} {};", dart_type(column.ty), column.name);
                }
            }
        }
        match language {
            SdkLanguage::Swift => {
                let _ = writeln!(output, "    public var {LIVENESS_COLUMN}: Bool\n}}");
            }
            SdkLanguage::Kotlin => {
                let _ = writeln!(output, "    val {LIVENESS_COLUMN}: Boolean = false,\n)");
            }
            SdkLanguage::TypeScript => {
                let _ = writeln!(output, "  {LIVENESS_COLUMN}: boolean;\n}}");
            }
            SdkLanguage::Dart => {
                let _ = writeln!(output, "  final bool {LIVENESS_COLUMN};\n");
                output.push_str("  const ");
                output.push_str(&type_name);
                output.push_str("({required this.id");
                for column in table.columns() {
                    let _ = write!(output, ", required this.{}", column.name);
                }
                let _ = writeln!(output, ", this.{LIVENESS_COLUMN} = false}});\n}}");
            }
        }
        files.insert(model_file_name(&type_name, language), output);
    }
    files.insert(
        client_file_name(language).to_owned(),
        client_contract(language, contract.fingerprint()),
    );
    GeneratedSdk { language, files }
}

fn client_contract(language: SdkLanguage, fingerprint: u64) -> String {
    match language {
        SdkLanguage::Swift => format!(
            "import Foundation\n\npublic let loomabaseSchemaFingerprint: UInt64 = {fingerprint}\n\npublic protocol LoomabaseTransport: Sendable {{\n    func sync(payload: Data) async throws -> Data\n}}\n"
        ),
        SdkLanguage::Kotlin => format!(
            "package dev.loomabase.generated\n\nconst val LOOMABASE_SCHEMA_FINGERPRINT: ULong = {fingerprint}u\n\nfun interface LoomabaseTransport {{\n    suspend fun sync(payload: ByteArray): ByteArray\n}}\n"
        ),
        SdkLanguage::TypeScript => format!(
            "export const LOOMABASE_SCHEMA_FINGERPRINT = \"{fingerprint}\";\n\nexport interface LoomabaseTransport {{\n  sync(payload: Uint8Array): Promise<Uint8Array>;\n}}\n"
        ),
        SdkLanguage::Dart => format!(
            "const String loomabaseSchemaFingerprint = '{fingerprint}';\n\nabstract interface class LoomabaseTransport {{\n  Future<List<int>> sync(List<int> payload);\n}}\n"
        ),
    }
}

fn model_file_name(type_name: &str, language: SdkLanguage) -> String {
    match language {
        SdkLanguage::Swift => format!("{type_name}.swift"),
        SdkLanguage::Kotlin => format!("{type_name}.kt"),
        SdkLanguage::TypeScript => format!("{}.ts", type_name.to_lowercase()),
        SdkLanguage::Dart => format!("{}.dart", snake_case(type_name)),
    }
}

fn client_file_name(language: SdkLanguage) -> &'static str {
    match language {
        SdkLanguage::Swift => "LoomabaseTransport.swift",
        SdkLanguage::Kotlin => "LoomabaseTransport.kt",
        SdkLanguage::TypeScript => "loomabase_transport.ts",
        SdkLanguage::Dart => "loomabase_transport.dart",
    }
}

fn swift_type(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::Text => "String",
        ColumnType::Integer => "Int64",
        ColumnType::Real => "Double",
        ColumnType::Boolean => "Bool",
    }
}

fn kotlin_type(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::Text => "String",
        ColumnType::Integer => "Long",
        ColumnType::Real => "Double",
        ColumnType::Boolean => "Boolean",
    }
}

fn typescript_type(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::Text => "string",
        ColumnType::Integer | ColumnType::Real => "number",
        ColumnType::Boolean => "boolean",
    }
}

fn dart_type(ty: ColumnType) -> &'static str {
    match ty {
        ColumnType::Text => "String",
        ColumnType::Integer => "int",
        ColumnType::Real => "double",
        ColumnType::Boolean => "bool",
    }
}

fn pascal_case(value: &str) -> String {
    value
        .split('_')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            chars
                .next()
                .map(char::to_uppercase)
                .into_iter()
                .flatten()
                .chain(chars)
                .collect::<String>()
        })
        .collect()
}

fn snake_case(value: &str) -> String {
    let mut output = String::new();
    for (index, character) in value.chars().enumerate() {
        if index > 0 && character.is_uppercase() {
            output.push('_');
        }
        output.extend(character.to_lowercase());
    }
    output
}

const _: &str = LIVENESS_COLUMN;
