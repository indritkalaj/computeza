//! Build script: compile `.proto` to Rust via protox (pure-Rust
//! protoc replacement) + tonic-build. Avoids the host-protoc dep so
//! a virgin Ubuntu runner can build the crate without
//! `apt install protobuf-compiler`.

fn main() {
    let proto_root = "proto";
    let proto_files = ["proto/channel_partner.proto"];

    // Tell cargo to rerun when the protos change.
    for f in &proto_files {
        println!("cargo:rerun-if-changed={f}");
    }
    println!("cargo:rerun-if-changed=build.rs");

    let file_descriptors = protox::compile(proto_files, [proto_root])
        .expect("protox failed to compile the channel_partner.proto file");

    // tonic 0.14 split codegen into tonic_prost_build (prost
    // messages) + tonic_build (service traits). The combined
    // tonic-prost-build crate runs both in one shot.
    tonic_prost_build::configure()
        .build_client(true)
        .build_server(true)
        .compile_fds(file_descriptors)
        .expect("tonic_prost_build codegen failed");
}
