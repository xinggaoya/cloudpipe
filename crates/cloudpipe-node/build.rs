//! Build script: registers the `napi` symbols the cdylib must export.

fn main() {
    napi_build::setup();
}
