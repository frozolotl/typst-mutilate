use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, BufRead, BufReader, Read, Write},
    path::PathBuf,
};

use argh::FromArgs;
use ecow::{EcoString, EcoVec};
use hypher::Lang;
use rand::{seq::SliceRandom, SeedableRng};
use rand_xoshiro::Xoshiro256PlusPlus;
use typst_syntax::{ast, SyntaxKind, SyntaxNode};

/// A tool to replace all words in a typst document with random garbage.
#[derive(FromArgs)]
struct Args {
    /// a file to perform in-place replacement on
    #[argh(option, short = 'i')]
    in_place: Option<PathBuf>,
    /// the path to a line-separated wordlist
    #[argh(option, short = 'w')]
    wordlist: Option<PathBuf>,
    /// an ISO 639-1 language code, like `de`
    #[argh(option, short = 'l', default = r#"String::from("en")"#)]
    language: String,
    /// whether to replace elements that are more likely to change behavior, like strings
    #[argh(switch, short = 'a')]
    aggressive: bool,
}

fn main() -> io::Result<()> {
    let args: Args = argh::from_env();

    let mut code = String::new();
    if let Some(path) = &args.in_place {
        code = std::fs::read_to_string(path)?;
    } else {
        std::io::stdin().read_to_string(&mut code)?;
    }

    let mut context = build_context(&args)?;

    let syntax = typst_syntax::parse(&code);
    let errors = syntax.errors();
    if !errors.is_empty() {
        eprintln!("Syntax errors: {:?}", errors);
        return Ok(());
    }

    let mut output = Vec::new();
    mutilate(&syntax, &mut context, &mut output)?;
    if let Some(path) = &args.in_place {
        std::fs::write(path, &output)?;
    } else {
        std::io::stdout().write_all(&output)?;
    }

    Ok(())
}

struct Context {
    rng: Xoshiro256PlusPlus,
    aggressive: bool,
    language: Lang,
    by_length: BTreeMap<usize, Vec<EcoString>>,
    by_hyphenation: BTreeMap<EcoVec<u8>, Vec<EcoString>>,
}

fn build_context(args: &Args) -> io::Result<Context> {
    let rng = Xoshiro256PlusPlus::from_rng(rand::thread_rng()).unwrap();

    let language = {
        if args.language.len() != 2 {
            panic!("Language is not two ascii characters long.");
        }
        let bytes = args.language.as_bytes();
        Lang::from_iso([bytes[0], bytes[1]]).expect("language not supported")
    };

    let mut by_length: BTreeMap<usize, Vec<EcoString>> = BTreeMap::new();
    let mut by_hyphenation: BTreeMap<EcoVec<u8>, Vec<EcoString>> = BTreeMap::new();
    if let Some(path) = &args.wordlist {
        let mut reader = BufReader::new(File::open(path)?);
        let mut line = String::new();
        while reader.read_line(&mut line)? != 0 {
            let word = EcoString::from(line.trim_end());
            by_length
                .entry(word.chars().count())
                .or_default()
                .push(word.clone());
            let hyphenation = hypher::hyphenate(&word, language)
                .map(|syllable| syllable.chars().count().try_into().unwrap_or(u8::MAX))
                .collect();
            by_hyphenation.entry(hyphenation).or_default().push(word);
            line.clear();
        }
    }

    Ok(Context {
        rng,
        aggressive: args.aggressive,
        language,
        by_length,
        by_hyphenation,
    })
}

fn mutilate<W: Write>(
    syntax: &SyntaxNode,
    context: &mut Context,
    output: &mut W,
) -> io::Result<()> {
    match syntax.kind() {
        SyntaxKind::Text => mutilate_text(syntax.text(), context, output),
        SyntaxKind::LineComment => {
            write!(output, "//")?;
            let content = &syntax.text()[2..];
            mutilate_text(content, context, output)?;
            Ok(())
        }
        SyntaxKind::BlockComment => {
            write!(output, "/*")?;
            let content = &syntax.text()[2..syntax.text().len() - 2];
            mutilate_text(content, context, output)?;
            write!(output, "*/")?;
            Ok(())
        }
        SyntaxKind::Str if context.aggressive => {
            write!(output, "\"")?;
            let content = &syntax.text()[1..syntax.text().len() - 1];
            mutilate_text(content, context, output)?;
            write!(output, "\"")?;
            Ok(())
        }
        SyntaxKind::Raw => {
            let raw: ast::Raw = syntax.cast().unwrap();
            let backticks = syntax.text().split(|c| c != '`').next().unwrap();
            write!(output, "{backticks}")?;

            let mut text = syntax
                .text()
                .trim_start_matches('`')
                .strip_suffix(backticks)
                .unwrap();
            if let Some(lang) = raw.lang() {
                text = text.strip_prefix(lang).unwrap();
                write!(output, "{lang}")?;
            }

            mutilate_text(text, context, output)?;
            write!(output, "{backticks}")?;
            Ok(())
        }
        SyntaxKind::Link => mutilate_text(syntax.text(), context, output),
        SyntaxKind::ModuleInclude | SyntaxKind::ModuleImport => write_node(syntax, output),
        _ if syntax.children().next().is_some() => {
            for child in syntax.children() {
                mutilate(child, context, output)?;
            }
            Ok(())
        }
        _ => write_node(syntax, output),
    }
}

fn write_node<W: Write>(syntax: &SyntaxNode, output: &mut W) -> io::Result<()> {
    if syntax.children().next().is_some() {
        for child in syntax.children() {
            write_node(child, output)?;
        }
    } else {
        write!(output, "{}", syntax.text())?;
    }
    Ok(())
}

fn mutilate_text<W: Write>(text: &str, context: &mut Context, output: &mut W) -> io::Result<()> {
    let mut remaining = text;
    loop {
        let split = |c: char| !c.is_alphanumeric();
        let next_remaining = remaining.trim_start_matches(split);
        let Some(word) = next_remaining.split(split).find(|s| !s.is_empty()) else {
            break;
        };
        let whitespace = &remaining[..remaining.len() - next_remaining.len()];
        remaining = &next_remaining[word.len()..];

        write!(output, "{whitespace}")?;
        mutilate_word(word, context, output)?;
    }
    write!(output, "{remaining}")?;
    Ok(())
}

/// The minimum number of words that have to be available in a list in order to choose an item.
const MINIMUM_WORD_COUNT: usize = 16;

const CHARSET_TEXT: &[char] = &[
    'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm', 'n', 'o', 'p', 'q', 'r', 's',
    't', 'u', 'v', 'w', 'x', 'y', 'z', 'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L',
    'M', 'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z',
];
const CHARSET_DIGITS: &[char] = &['0', '1', '2', '3', '4', '5', '6', '7', '8', '9'];

fn mutilate_word<W: Write>(word: &str, context: &mut Context, output: &mut W) -> io::Result<()> {
    let length = word.chars().count();
    if word.chars().all(|c| c.is_numeric()) {
        let digit = CHARSET_DIGITS.choose(&mut context.rng).unwrap();
        write!(output, "{digit}")?;
        return Ok(());
    }

    // Find a word with the same hyphenation pattern.
    let hyphenation: EcoVec<u8> = hypher::hyphenate(word, context.language)
        .map(|syllable| syllable.chars().count().try_into().unwrap_or(u8::MAX))
        .collect();
    if let Some(words) = context.by_hyphenation.get(&hyphenation) {
        if words.len() >= MINIMUM_WORD_COUNT {
            if let Some(word) = words.choose(&mut context.rng) {
                return write!(output, "{word}");
            }
        }
    }

    if let Some(words) = context.by_length.get(&length) {
        if words.len() >= MINIMUM_WORD_COUNT {
            if let Some(word) = words.choose(&mut context.rng) {
                return write!(output, "{word}");
            }
        }
    }

    for _ in 0..length {
        write!(output, "{}", CHARSET_TEXT.choose(&mut context.rng).unwrap())?;
    }

    Ok(())
}
