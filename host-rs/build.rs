//! Mete en el ejecutable el icono de XtendedView y su nombre (el que muestran el Explorador y el
//! Administrador de tareas). Usa rc.exe del SDK de Windows.
fn main() {
    println!("cargo:rerun-if-changed=xtendedview.rc");
    println!("cargo:rerun-if-changed=../assets/xtendedview.ico");
    embed_resource::compile("xtendedview.rc", embed_resource::NONE)
        .manifest_optional()
        .expect("no se pudieron compilar los recursos (icono) con rc.exe");
}
