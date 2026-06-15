use std::env;

const PROD_VIRTUAL: &str = "virt1-1-1";
const PROD_LIST: &[&str] = &[PROD_VIRTUAL, "bln1-2512-1", "bln2-2512-1"];

fn main() {
    // Compile protobuf schema to Rust code with serde attributes
    prost_build::Config::new()
        .type_attribute(
            ".",
            "#[derive(Deserialize,Serialize)] #[serde(rename_all = \"snake_case\")]",
        )
        .compile_protos(
            &["assets/proto_schema/ledtransit_client.proto"],
            &["assets/proto_schema"],
        )
        .expect("Failed to compile proto files");

    // Get product to build for from environment
    let product = env::var("PRODUCT").unwrap_or_else(|_| {
        println!(
            "Environment variable PRODUCT not set. Please specify product with PRODUCT=<...> as one of: {:?}",
            PROD_LIST
        );
        PROD_VIRTUAL.to_string()
    });
    assert!(
        PROD_LIST.contains(&product.as_str()),
        "PRODUCT not recognized. Specify product with PRODUCT=<...> as one of: {:?}",
        PROD_LIST
    );

    // Extract versioning parts from product string
    let product_parts: Vec<&str> = product.split('-').collect();
    assert!(
        product_parts.len() == 3,
        "PRODUCT format invalid. Expected format: <PRODUCT>-<MAJOR>-<MINOR>"
    );
    let (product_group, hw_major, hw_minor) =
        (product_parts[0], product_parts[1], product_parts[2]);

    // Rebuild if environment or proto schema changes
    println!("cargo:rerun-if-env-changed=RELEASE");
    println!("cargo:rerun-if-env-changed=PRODUCT");
    println!("cargo:rerun-if-env-changed=SSL_ENABLED");
    println!("cargo:rerun-if-changed=assets/proto_schema/ledtransit_client.proto");

    // Set Rust environment variables and attributes for product configuration
    println!("cargo:rustc-env=PRODUCT={}", product);
    println!("cargo:rustc-env=PRODUCT_GROUP={}", product_group);
    println!("cargo:rustc-env=HW_MAJOR={}", hw_major);
    println!("cargo:rustc-env=HW_MINOR={}", hw_minor);
    println!("cargo:rustc-check-cfg=cfg(ssl_enabled)");
    if env::var("SSL_ENABLED").ok().as_deref() == Some("true") {
        println!("cargo:rustc-cfg=ssl_enabled");
    }
}
