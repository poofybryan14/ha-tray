# HaTray — widget et icône de notification Home Assistant (Windows 11)

Application Rust en Win32 pur : pas de framework d'interface, pas de runtime async, pas de WebView.

## Architecture

```
ha-tray/
├── Cargo.toml            dépendances minimales + profil release « taille »
├── .cargo/config.toml    CRT statique => exe autonome
└── src/
    ├── main.rs           charge la config, initialise l'état, lance l'UI
    ├── config.rs         config.json + chiffrement DPAPI du jeton
    ├── state.rs          état partagé (entités, drapeaux)
    ├── ha.rs             WebSocket (état temps réel) + REST (actions)
    └── ui.rs             fenêtre widget, icône de notification, menu dynamique, dessin GDI
```

### Flux

1. Thread **ha-ws** : connexion WebSocket, authentification, `get_states` (une fois), puis
   abonnement `state_changed`. Lecture bloquante => le thread dort dans le noyau.
2. Chaque changement visible met à jour l'état partagé et poste `WM_STATE` à la fenêtre.
3. Thread **principal** : `GetMessage` bloquant. Le widget n'est repeint que s'il est visible.
4. Clic sur une lumière ou une scène : requête REST dans un thread éphémère ; le retour
   visuel arrive par le WebSocket (l'interface ne « devine » jamais l'état).

### Économie de ressources

| Mécanisme | Effet |
|---|---|
| Lecture WebSocket bloquante, aucun timer | 0 % CPU au repos |
| Ping seulement après 40 s de silence | détection de connexion morte sans trafic inutile |
| Filtrage immédiat des événements (light.* / scene.*) | pas d'allocation pour les capteurs |
| Aucun événement ignoré par comparaison d'état | pas de repaint inutile |
| GDI à la demande, polices créées puis détruites à chaque peinture | rien d'alloué au repos |
| `SetProcessWorkingSetSize(-1, -1)` après masquage et rechargement | working set rendu au système |
| Menu reconstruit à chaque clic droit | aucune structure de menu conservée |
| TLS natif Schannel, CRT statique, `opt-level = "z"`, LTO | exe de quelques centaines de Ko à ~2 Mo |

Ordre de grandeur attendu : quelques Mo de working set. **À mesurer chez vous** (voir plus bas).

## Configuration

Au premier lancement, `%APPDATA%\HaTray\config.json` est créé :

```json
{
  "url": "http://192.168.1.50:8123",
  "token": "COLLEZ_ICI_VOTRE_TOKEN_LONGUE_DUREE",
  "entities": []
}
```

- `url` : adresse de Home Assistant sur le NAS (`http://` ou `https://`).
- `token` : jeton d'accès longue durée (Home Assistant → votre profil → Sécurité →
  Jetons d'accès longue durée). Au lancement suivant il est **chiffré avec DPAPI**
  (lié à votre session Windows) puis effacé du fichier en clair.
- `entities` : liste blanche optionnelle, par exemple `["light.salon", "scene.soiree"]`.
  Vide = toutes les lumières et scènes.

Si vous utilisez HTTPS avec un certificat auto-signé, Windows doit faire confiance à ce
certificat (sinon utilisez `http://` sur le réseau local).

## Utilisation

- **Clic gauche sur l'icône** : affiche / masque le widget (bas droite de l'écran).
- **Clic droit** : menu dynamique (lumières avec ●/○, sous-menu Scènes, « Toujours visible », Quitter).
- **Widget** : clic sur une ligne = bascule (lumière) ou activation (scène) ; clic dans la
  barre sous une lumière = luminosité directe ; molette = ±10 % ; l'en-tête sert à déplacer.
- Sans « Toujours visible », le widget se rétracte dès qu'il perd le focus.

## Compilation

1. Installer **Rust** : https://rustup.rs (toolchain `stable-x86_64-pc-windows-msvc`).
2. Installer les **Build Tools for Visual Studio** avec la charge « Développement Desktop en C++ »
   (éditeur de liens MSVC et SDK Windows).
3. Dans le dossier du projet :

   ```powershell
   cargo build --release
   ```

4. Résultat : `target\release\ha-tray.exe`, un seul fichier, sans DLL à installer
   (CRT statique via `.cargo/config.toml`).

### Icône personnalisée (optionnel)

Ajoutez `winres` en `[build-dependencies]`, un `build.rs` qui appelle
`winres::WindowsResource::new().set_icon("icon.ico").compile()`, puis remplacez
`LoadIconW(None, IDI_APPLICATION)` dans `ui.rs` par `LoadIconW(hinst, PCWSTR(1 as _))`.

## Déploiement

1. Copier `ha-tray.exe` où vous voulez (par exemple `C:\Tools\HaTray\`).
2. Premier lancement => renseigner `config.json` => relancer.
3. Démarrage automatique avec Windows, **sans toucher au registre** : touche Win+R, tapez
   `shell:startup`, puis placez-y un raccourci vers `ha-tray.exe`.

## Installateur (Inno Setup)

Le script `installer\HaTray.iss` produit `HaTray-Setup.exe`, sans droits administrateur.
L'assistant propose :

- **Créer une icône sur le Bureau** (coché par défaut) ;
- **Lancer HaTray au démarrage de Windows** (décoché par défaut) ;
- lancer HaTray à la fin de l'installation.

Construction :

1. Installer Inno Setup 6 : https://jrsoftware.org/isdl.php
2. `cargo build --release`
3. Ouvrir `installer\HaTray.iss` dans Inno Setup, puis Build > Compile
   (ou `iscc installer\HaTray.iss`).
4. Récupérer `installer\output\HaTray-Setup.exe`.

L'installateur n'écrit pas d'entrée de désinstallation dans le registre
(`CreateUninstallRegKey=no`) : HaTray n'apparaît donc pas dans Paramètres > Applications.
Utilisez le raccourci « Désinstaller HaTray » du menu Démarrer. La désinstallation propose
aussi de supprimer `%APPDATA%\HaTray`.

## Lancement au démarrage

Option « Lancer HaTray au démarrage de Windows » dans la fenêtre Paramètres (clic droit sur
l'icône > Paramètres…). Elle crée ou supprime `HaTray.lnk` dans `shell:startup` : un fichier,
aucune clé de registre. Si vous déplacez `ha-tray.exe`, décochez puis recochez l'option.

## Désinstallation propre

Supprimez le raccourci du dossier `shell:startup`, `ha-tray.exe` et le dossier
`%APPDATA%\HaTray`. L'application n'écrit aucune clé de registre.

## Mesurer la consommation

- Gestionnaire des tâches → onglet Détails → colonne « Mémoire (jeu de travail privé) ».
- Ou en PowerShell :

  ```powershell
  Get-Process ha-tray | Select-Object Name, @{n='WorkingSetMo';e={[math]::Round($_.WorkingSet64/1MB,1)}}, CPU
  ```

## Pistes d'amélioration

- `subscribe_trigger` avec une liste d'entités pour que le serveur filtre lui-même les événements.
- Persistance de la position du widget dans `config.json`.
- Instance unique (`CreateMutexW`).
- Widget collé au bureau (sous les autres fenêtres) : remplacer `HWND_TOPMOST` par `HWND_BOTTOM`
  et retirer `WS_EX_TOPMOST`.
