//! Crate d'intégration rattachée à `src/bin/miryad.sdd` — exerce le contrat observable de
//! l'exécutable `miryad` depuis un vrai processus : codes de sortie `0` et `2` seulement, aide
//! et version sur stdout, refus des arguments inconnus sur stderr avec le code `2`, premier
//! flag d'affichage rencontré gagnant, et `Usage:` nommé d'après le basename de `argv[0]` sous
//! invocation renommée. L'exécutable est lancé par `std::process::Command` via
//! `env!("CARGO_BIN_EXE_miryad")` (variable posée par cargo pour les tests d'intégration) ; la
//! version attendue se déduit de `env!("CARGO_PKG_VERSION")`, jamais d'un littéral.

// Famille panic/unwrap/indexation tolérée dans cette crate de test : en-tête d'exemption
// équivalent à celui de `src/lib.rs`, posé d'après le `Must` de `tooling.sdd` — une crate
// d'intégration n'hérite pas des attributs de la librairie. Groupes `pedantic` et `cargo`
// restent `deny` sous `cfg(test)`.
#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::dbg_macro,
        clippy::todo,
        clippy::unimplemented,
        clippy::print_stdout,
        clippy::print_stderr,
        clippy::arithmetic_side_effects,
        clippy::indexing_slicing,
        clippy::unwrap_in_result,
        clippy::panic_in_result_fn
    )
)]

use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::process::Command;
use std::process::Output;

/// Texte de la ligne d'about que `clap` doit rendre : le commentaire de documentation de `Cli`
/// moins le `.` final, que `clap_derive` retire (`remove_period`, vérifié dans la pile résolue
/// `clap_derive 4.6.4` de /Cargo.lock — la structure vide n'a pas de `verbatim_doc_comment`).
///
/// Cette valeur verrouille le contrat amendé du 2026-10-03 dans `src/bin/miryad.sdd` (voie A,
/// arbitrée par Sébastien) : le `Must` et le `Then` du Scenario d'aide disent désormais ce
/// que `clap_derive` produit — le commentaire de `Cli` hors le point final consommé par
/// `remove_period`. La spec dit le réel.
const ABOUT: &str = "CLI de scaffolding miryad — génère une application depuis un modèle de données";

/// Ligne de version attendue : template `clap` `{name} {version}` + saut de ligne, `name` figé
/// par `#[command(name = "miryad")]` et `version` lue depuis `CARGO_PKG_VERSION` compilée dans
/// le binaire — jamais de littéral pour le numéro de version.
#[must_use]
fn expected_version_line() -> String {
    let version = env!("CARGO_PKG_VERSION");
    format!("miryad {version}\n")
}

/// Nom de programme que `clap` pose dans la ligne `Usage:` : le basename (racine de nom) de
/// `argv[0]`, donc celui du binaire tel que cargo l'a construit.
#[must_use]
fn invoked_program() -> String {
    let exe = Path::new(env!("CARGO_BIN_EXE_miryad"));
    exe.file_stem()
        .expect("le chemin du binaire miryad a un nom de fichier")
        .to_str()
        .expect("le nom du binaire miryad est en UTF-8")
        .to_owned()
}

/// Lance l'exécutable compilé par cargo avec `args`, stdout et stderr capturés.
#[must_use]
fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_miryad"))
        .args(args)
        .output()
        .expect("le binaire miryad compilé se lance")
}

/// Répertoire temporaire dédié à un test, vidé et recréé pour être vide à l'entrée.
#[must_use]
fn temp_dir(label: &str) -> PathBuf {
    let process = std::process::id();
    let dir = std::env::temp_dir().join(format!("miryad-core-cli-{label}-{process}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("répertoire temporaire créable");
    dir
}

/// Scenario « Lancement sans argument réussit sans rien faire » : code `0`, stdout et stderr
/// vides, et aucune sortie latérale — le `Then` « ni fichier écrit » est prouvé en lançant le
/// binaire dans un répertoire de travail vide, qui doit le rester.
#[test]
fn empty_invocation_is_silent_and_exits_0() {
    let workdir = temp_dir("empty_invocation");
    let output = Command::new(env!("CARGO_BIN_EXE_miryad"))
        .current_dir(&workdir)
        .output()
        .expect("le binaire miryad compilé se lance");
    let created: Vec<PathBuf> = std::fs::read_dir(&workdir)
        .expect("répertoire de travail lisible")
        .map(|entry| entry.expect("entrée du répertoire de travail lisible").path())
        .collect();
    std::fs::remove_dir_all(&workdir).expect("nettoyage du répertoire de travail");

    assert_eq!(output.status.code(), Some(0), "argv vide doit sortir `0`");
    assert!(
        output.stdout.is_empty(),
        "stdout vide, réel : {:?}",
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        output.stderr.is_empty(),
        "stderr vide, réel : {:?}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        created.is_empty(),
        "aucun fichier écrit dans le répertoire de travail : {created:?}"
    );
}

/// Scenario « --help affiche l'about du squelette sur stdout » : code `0`, stderr vide, ligne
/// d'about en tête puis ligne `Usage:` nommant le programme appelé, bloc `Options:` listant
/// `-h, --help` avec `Print help` sans mention d'aide longue, et `-V, --version` avec
/// `Print version`. `-h` rend le même texte que `--help`.
#[test]
fn help_prints_about_and_flags_on_stdout_exit_0() {
    let output = run(&["--help"]);
    let short = run(&["-h"]);

    assert_eq!(output.status.code(), Some(0), "`--help` doit sortir `0`");
    assert_eq!(short.status.code(), Some(0), "`-h` doit sortir `0`");
    assert!(output.stderr.is_empty(), "`--help` ne rien écrire sur stderr");
    assert!(short.stderr.is_empty(), "`-h` ne rien écrire sur stderr");
    assert_eq!(
        short.stdout, output.stdout,
        "`-h` rend le même texte que `--help`"
    );

    let stdout = String::from_utf8(output.stdout).expect("l'aide sur stdout est en UTF-8");
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(
        lines.first().copied().expect("l'aide porte une ligne d'about"),
        ABOUT,
        "première ligne = texte du commentaire de `Cli` hors point final (amendé 2026-10-03)"
    );
    assert!(
        lines.iter().skip(1).any(|line| line.starts_with("Usage:")),
        "une ligne `Usage:` suit la ligne d'about, réel : {stdout}"
    );
    let program = invoked_program();
    let expected_usage = format!("Usage: {program}");
    assert!(
        lines.iter().any(|line| *line == expected_usage),
        "la ligne `Usage:` nomme le programme appelé ({program}), réel : {stdout}"
    );

    assert!(stdout.contains("Options:"), "l'aide porte un bloc `Options:`");
    let help_line = stdout
        .lines()
        .find(|line| line.contains("-h, --help"))
        .expect("`-h, --help` est listé dans l'aide");
    assert!(
        help_line.contains("Print help"),
        "texte `Print help` auprès de `-h, --help`"
    );
    let version_flag_line = stdout
        .lines()
        .find(|line| line.contains("-V, --version"))
        .expect("`-V, --version` est listé dans l'aide");
    assert!(
        version_flag_line.contains("Print version"),
        "texte `Print version` auprès de `-V, --version`"
    );
    assert!(
        !stdout.contains("see more"),
        "structure vide : aucune mention d'aide longue `see more`, réel : {stdout}"
    );
}

/// Scenario « --version rend la version du paquet » : code `0`, stdout exactement
/// `miryad <version>` + saut de ligne avec la version `CARGO_PKG_VERSION` compilée — jamais un
/// littéral — , stderr vide ; `-V` rend le même texte que `--version`.
#[test]
fn version_prints_package_version_exit_0() {
    let output = run(&["--version"]);
    let short = run(&["-V"]);
    let expected = expected_version_line();

    assert_eq!(output.status.code(), Some(0), "`--version` doit sortir `0`");
    assert_eq!(short.status.code(), Some(0), "`-V` doit sortir `0`");
    assert!(output.stderr.is_empty(), "`--version` ne rien écrire sur stderr");
    assert!(short.stderr.is_empty(), "`-V` ne rien écrire sur stderr");
    assert_eq!(
        String::from_utf8(output.stdout).expect("la version sur stdout est en UTF-8"),
        expected,
        "stdout exactement `miryad ` + `CARGO_PKG_VERSION` + saut de ligne"
    );
    assert_eq!(
        String::from_utf8(short.stdout).expect("la version sur stdout est en UTF-8"),
        expected,
        "`-V` rend le même texte que `--version`"
    );
}

/// Scenario « Un argument inconnu est refusé avec le code 2 » : positionnel (`generate`) ou
/// option (`--model`), même classe de refus — message contenant `unexpected argument` sur
/// stderr, code `2`, stdout vide.
#[test]
fn unknown_argument_exits_2_on_stderr() {
    for argument in ["generate", "--model"] {
        let output = run(&[argument]);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

        assert_eq!(output.status.code(), Some(2), "`{argument}` doit sortir `2`");
        assert!(
            stderr.contains("unexpected argument"),
            "stderr porte `unexpected argument` pour `{argument}`, réel : {stderr}"
        );
        assert!(
            stdout.is_empty(),
            "stdout reste vide pour `{argument}`, réel : {stdout}"
        );
    }
}

/// Scenario « Le premier flag d'affichage rencontré gagne » : `--version generate` rend la
/// version avec le code `0` ; `generate --version` échoue `2` avec stdout vide avant d'examiner
/// `--version` ; `--help --version` rend l'aide et non la version.
#[test]
fn first_display_flag_wins_over_later_arguments() {
    let version = run(&["--version", "generate"]);
    assert_eq!(
        version.status.code(),
        Some(0),
        "`--version generate` : le flag rencontré d'abord gagne, code `0`"
    );
    assert_eq!(
        String::from_utf8_lossy(&version.stdout),
        expected_version_line(),
        "`--version generate` rend la ligne de version"
    );
    assert!(
        version.stderr.is_empty(),
        "`--version generate` ne rien écrire sur stderr"
    );

    let rejected = run(&["generate", "--version"]);
    assert_eq!(
        rejected.status.code(),
        Some(2),
        "`generate --version` : l'argument inconnu rencontré le premier échoue `2`"
    );
    assert!(
        rejected.stdout.is_empty(),
        "`generate --version` laisse stdout vide, réel : {:?}",
        String::from_utf8_lossy(&rejected.stdout)
    );

    let help = run(&["--help"]);
    let combined = run(&["--help", "--version"]);
    assert_eq!(
        combined.status.code(),
        Some(0),
        "`--help --version` : le premier flag d'affichage gagne, code `0`"
    );
    assert_eq!(
        combined.stdout, help.stdout,
        "`--help` premier rend exactement le texte de l'aide"
    );
    let combined_stdout = String::from_utf8(combined.stdout).expect("l'aide sur stdout est en UTF-8");
    let version = env!("CARGO_PKG_VERSION");
    assert!(
        !combined_stdout.contains(version),
        "l'aide rendue ne porte pas la version `{version}`, réel : {combined_stdout}"
    );
}

/// Scenario « Renommé, l'aide suit l'appel, la version reste miryad » : copie du binaire
/// rebaptisée `miryad-scaffold` dans un répertoire temporaire (le Given exact du Scenario) ;
/// sous ce nom, la ligne `Usage:` affiche `miryad-scaffold` (basename d'`argv[0]`) alors que
/// `--version` commence par `miryad ` (template `name` figé), les deux avec le code `0`.
#[test]
fn renamed_invocation_changes_usage_only() {
    let workdir = temp_dir("renamed_invocation");
    let renamed = workdir.join("miryad-scaffold");
    // Copie écrite à la main puis `sync_all` avant l'exec : `fs::copy` rendait le fichier sans
    // drainer l'writeback, et sous XFS le premier exec de la copie échouait de façon sporadique
    // avec `ETXTBSY` (`Os { code: 26, kind: ExecutableFileBusy }`, mesuré 2/25 avant correction —
    // validator B5h). Le bloc ferme le descripteur d'écriture avant l'exec ; `set_permissions`
    // reproduit le mode du binaire source que `fs::copy` copiait (bit exécutable préservé).
    {
        let source = env!("CARGO_BIN_EXE_miryad");
        let mode = std::fs::metadata(source)
            .expect("le binaire miryad a un mode")
            .permissions();
        let binary = std::fs::read(source).expect("lecture des octets du binaire miryad");
        let mut copy = std::fs::File::create(&renamed).expect("copie du binaire créable");
        copy.write_all(&binary)
            .expect("écriture des octets dans la copie");
        copy.sync_all()
            .expect("writeback drainé avant exec (ETXTBSY/XFS)");
        std::fs::set_permissions(&renamed, mode).expect("mode exécutable copié sur la copie");
    }

    let help = Command::new(&renamed)
        .arg("--help")
        .output()
        .expect("le binaire renommé se lance");
    let version = Command::new(&renamed)
        .arg("--version")
        .output()
        .expect("le binaire renommé se lance");
    std::fs::remove_dir_all(&workdir).expect("nettoyage du répertoire temporaire de la copie");

    assert_eq!(help.status.code(), Some(0), "`--help` renommé doit sortir `0`");
    assert_eq!(
        version.status.code(),
        Some(0),
        "`--version` renommé doit sortir `0`"
    );
    assert!(
        help.stderr.is_empty(),
        "`--help` renommé ne rien écrire sur stderr"
    );
    assert!(
        version.stderr.is_empty(),
        "`--version` renommé ne rien écrire sur stderr"
    );

    let help_stdout = String::from_utf8(help.stdout).expect("l'aide sur stdout est en UTF-8");
    let usage = help_stdout
        .lines()
        .find(|line| line.starts_with("Usage:"))
        .expect("l'aide renommée porte une ligne `Usage:`");
    assert_eq!(
        usage, "Usage: miryad-scaffold",
        "la ligne `Usage:` suit le basename d'`argv[0]`, pas l'attribut `name`"
    );

    let version_stdout = String::from_utf8(version.stdout).expect("la version sur stdout est en UTF-8");
    assert!(
        version_stdout.starts_with("miryad "),
        "la version commence par `miryad ` — template `name` figé, réel : {version_stdout}"
    );
    assert!(
        !version_stdout.contains("miryad-scaffold"),
        "la version ignore `argv[0]` : aucun `miryad-scaffold` dedans, réel : {version_stdout}"
    );
    assert_eq!(
        version_stdout,
        expected_version_line(),
        "la version renommée reste la ligne `miryad <version>` du paquet"
    );
}
