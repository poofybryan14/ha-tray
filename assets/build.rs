// Embarque l'icône (assets/icon.ico) dans ha-tray.exe : elle sert à l'Explorateur,
// aux raccourcis, à l'icône de la zone de notification et à la fenêtre de configuration.
fn main() {
    println!("cargo:rerun-if-changed=assets/icon.ico");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("assets/icon.ico"); // devient la ressource d'icône n°1
        res.set("FileDescription", "HaTray - Home Assistant");
        res.set("ProductName", "HaTray");
        res.compile().expect("échec de l'intégration de l'icône (rc.exe introuvable ?)");
    }
}
