#![doc(html_root_url = "https://docs.rs/wasm-bindgen-cli-support/0.2")]

use failure::{bail, Error, ResultExt};
use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::mem;
use std::path::{Path, PathBuf};
use std::str;
use walrus::Module;

mod decode;
mod descriptor;
mod js;
pub mod wasm2es6js;

pub struct Bindgen {
    input: Input,
    out_name: Option<String>,
    nodejs: bool,
    nodejs_experimental_modules: bool,
    browser: bool,
    no_modules: bool,
    no_modules_global: Option<String>,
    debug: bool,
    typescript: bool,
    demangle: bool,
    keep_debug: bool,
    remove_name_section: bool,
    emit_start: bool,
    // Experimental support for `WeakRefGroup`, an upcoming ECMAScript feature.
    // Currently only enable-able through an env var.
    weak_refs: bool,
    // Experimental support for the wasm threads proposal, transforms the wasm
    // module to be "ready to be instantiated on any thread"
    threads: Option<wasm_bindgen_threads_xform::Config>,
}

enum Input {
    Path(PathBuf),
    Module(Module, String),
    None,
}

impl Bindgen {
    pub fn new() -> Bindgen {
        Bindgen {
            input: Input::None,
            out_name: None,
            nodejs: false,
            nodejs_experimental_modules: false,
            browser: false,
            no_modules: false,
            no_modules_global: None,
            debug: false,
            typescript: false,
            demangle: true,
            keep_debug: false,
            remove_name_section: false,
            emit_start: true,
            weak_refs: env::var("WASM_BINDGEN_WEAKREF").is_ok(),
            threads: threads_config(),
        }
    }

    pub fn input_path<P: AsRef<Path>>(&mut self, path: P) -> &mut Bindgen {
        self.input = Input::Path(path.as_ref().to_path_buf());
        self
    }

    pub fn out_name(&mut self, name: &str) -> &mut Bindgen {
        self.out_name = Some(name.to_string());
        self
    }

    /// Explicitly specify the already parsed input module.
    pub fn input_module(&mut self, name: &str, module: Module) -> &mut Bindgen {
        let name = name.to_string();
        self.input = Input::Module(module, name);
        return self;
    }

    pub fn nodejs(&mut self, node: bool) -> &mut Bindgen {
        self.nodejs = node;
        self
    }

    pub fn nodejs_experimental_modules(&mut self, node: bool) -> &mut Bindgen {
        self.nodejs_experimental_modules = node;
        self
    }

    pub fn browser(&mut self, browser: bool) -> &mut Bindgen {
        self.browser = browser;
        self
    }

    pub fn no_modules(&mut self, no_modules: bool) -> &mut Bindgen {
        self.no_modules = no_modules;
        self
    }

    pub fn no_modules_global(&mut self, name: &str) -> &mut Bindgen {
        self.no_modules_global = Some(name.to_string());
        self
    }

    pub fn debug(&mut self, debug: bool) -> &mut Bindgen {
        self.debug = debug;
        self
    }

    pub fn typescript(&mut self, typescript: bool) -> &mut Bindgen {
        self.typescript = typescript;
        self
    }

    pub fn demangle(&mut self, demangle: bool) -> &mut Bindgen {
        self.demangle = demangle;
        self
    }

    pub fn keep_debug(&mut self, keep_debug: bool) -> &mut Bindgen {
        self.keep_debug = keep_debug;
        self
    }

    pub fn remove_name_section(&mut self, remove: bool) -> &mut Bindgen {
        self.remove_name_section = remove;
        self
    }

    pub fn emit_start(&mut self, emit: bool) -> &mut Bindgen {
        self.emit_start = emit;
        self
    }

    pub fn generate<P: AsRef<Path>>(&mut self, path: P) -> Result<(), Error> {
        self._generate(path.as_ref())
    }

    fn _generate(&mut self, out_dir: &Path) -> Result<(), Error> {
        let (mut module, stem) = match self.input {
            Input::None => bail!("must have an input by now"),
            Input::Module(ref mut m, ref name) => {
                let blank_module = Module::default();
                (mem::replace(m, blank_module), &name[..])
            }
            Input::Path(ref path) => {
                let contents = fs::read(&path)
                    .with_context(|_| format!("failed to read `{}`", path.display()))?;
                let module = walrus::ModuleConfig::new()
                    // Skip validation of the module as LLVM's output is
                    // generally already well-formed and so we won't gain much
                    // from re-validating. Additionally LLVM's current output
                    // for threads includes atomic instructions but doesn't
                    // include shared memory, so it fails that part of
                    // validation!
                    .strict_validate(false)
                    .parse(&contents)
                    .context("failed to parse input file as wasm")?;
                let stem = match &self.out_name {
                    Some(name) => &name,
                    None => path.file_stem().unwrap().to_str().unwrap(),
                };
                (module, stem)
            }
        };
        let mut program_storage = Vec::new();
        let programs = extract_programs(&mut module, &mut program_storage)
            .with_context(|_| "failed to extract wasm-bindgen custom sections")?;

        if let Some(cfg) = &self.threads {
            cfg.run(&mut module)
                .with_context(|_| "failed to prepare module for threading")?;
        }

        if self.demangle {
            demangle(&mut module);
        }

        // Here we're actually instantiating the module we've parsed above for
        // execution. Why, you might be asking, are we executing wasm code? A
        // good question!
        //
        // Transmitting information from `#[wasm_bindgen]` here to the CLI tool
        // is pretty tricky. Specifically information about the types involved
        // with a function signature (especially generic ones) can be hefty to
        // translate over. As a result, the macro emits a bunch of shims which,
        // when executed, will describe to us what the types look like.
        //
        // This means that whenever we encounter an import or export we'll
        // execute a shim function which informs us about its type so we can
        // then generate the appropriate bindings.
        let mut instance = wasm_bindgen_wasm_interpreter::Interpreter::new(&module);

        let mut memories = module.memories.iter().map(|m| m.id());
        let memory = memories.next();
        if memories.next().is_some() {
            bail!("multiple memories currently not supported");
        }
        drop(memories);
        let memory = memory.unwrap_or_else(|| module.memories.add_local(false, 1, None));

        let (js, ts) = {
            let mut cx = js::Context {
                globals: String::new(),
                imports: String::new(),
                imports_post: String::new(),
                footer: String::new(),
                typescript: format!("/* tslint:disable */\n"),
                exposed_globals: Some(Default::default()),
                required_internal_exports: Default::default(),
                imported_names: Default::default(),
                imported_identifiers: Default::default(),
                exported_classes: Some(Default::default()),
                config: &self,
                module: &mut module,
                function_table_needed: false,
                interpreter: &mut instance,
                memory,
                imported_functions: Default::default(),
                imported_statics: Default::default(),
                direct_imports: Default::default(),
                start: None,
            };
            for program in programs.iter() {
                js::SubContext {
                    program,
                    cx: &mut cx,
                    vendor_prefixes: Default::default(),
                }
                .generate()?;
            }
            cx.finalize(stem)?
        };

        let extension = if self.nodejs_experimental_modules {
            "mjs"
        } else {
            "js"
        };
        let js_path = out_dir.join(stem).with_extension(extension);
        fs::write(&js_path, reset_indentation(&js))
            .with_context(|_| format!("failed to write `{}`", js_path.display()))?;

        if self.typescript {
            let ts_path = js_path.with_extension("d.ts");
            fs::write(&ts_path, ts)
                .with_context(|_| format!("failed to write `{}`", ts_path.display()))?;
        }

        let wasm_path = out_dir.join(format!("{}_bg", stem)).with_extension("wasm");

        if self.nodejs {
            let js_path = wasm_path.with_extension(extension);
            let shim = self.generate_node_wasm_import(&module, &wasm_path);
            fs::write(&js_path, shim)
                .with_context(|_| format!("failed to write `{}`", js_path.display()))?;
        }

        if self.typescript {
            let ts_path = wasm_path.with_extension("d.ts");
            let ts = wasm2es6js::typescript(&module)?;
            fs::write(&ts_path, ts)
                .with_context(|_| format!("failed to write `{}`", ts_path.display()))?;
        }

        let wasm_bytes = module.emit_wasm()?;
        fs::write(&wasm_path, wasm_bytes)
            .with_context(|_| format!("failed to write `{}`", wasm_path.display()))?;

        Ok(())
    }

    fn generate_node_wasm_import(&self, m: &Module, path: &Path) -> String {
        let mut imports = BTreeSet::new();
        for import in m.imports.iter() {
            imports.insert(&import.module);
        }

        let mut shim = String::new();

        if self.nodejs_experimental_modules {
            for (i, module) in imports.iter().enumerate() {
                shim.push_str(&format!("import * as import{} from '{}';\n", i, module));
            }
            // On windows skip the leading `/` which comes out when we parse a
            // url to use `C:\...` instead of `\C:\...`
            shim.push_str(&format!(
                "
                import * as path from 'path';
                import * as fs from 'fs';
                import * as url from 'url';
                import * as process from 'process';

                let file = path.dirname(url.parse(import.meta.url).pathname);
                if (process.platform === 'win32') {{
                    file = file.substring(1);
                }}
                const bytes = fs.readFileSync(path.join(file, '{}'));
            ",
                path.file_name().unwrap().to_str().unwrap()
            ));
        } else {
            shim.push_str(&format!(
                "
                const path = require('path').join(__dirname, '{}');
                const bytes = require('fs').readFileSync(path);
            ",
                path.file_name().unwrap().to_str().unwrap()
            ));
        }
        shim.push_str("let imports = {};\n");
        for (i, module) in imports.iter().enumerate() {
            if self.nodejs_experimental_modules {
                shim.push_str(&format!("imports['{}'] = import{};\n", module, i));
            } else {
                shim.push_str(&format!("imports['{0}'] = require('{0}');\n", module));
            }
        }

        shim.push_str(&format!(
            "
                const wasmModule = new WebAssembly.Module(bytes);
                const wasmInstance = new WebAssembly.Instance(wasmModule, imports);
            ",
        ));

        if self.nodejs_experimental_modules {
            for entry in m.exports.iter() {
                shim.push_str("export const ");
                shim.push_str(&entry.name);
                shim.push_str(" = wasmInstance.exports.");
                shim.push_str(&entry.name);
                shim.push_str(";\n");
            }
        } else {
            shim.push_str("module.exports = wasmInstance.exports;\n");
        }

        reset_indentation(&shim)
    }
}

fn extract_programs<'a>(
    module: &mut Module,
    program_storage: &'a mut Vec<Vec<u8>>,
) -> Result<Vec<decode::Program<'a>>, Error> {
    let my_version = wasm_bindgen_shared::version();
    let mut to_remove = Vec::new();
    assert!(program_storage.is_empty());

    for (i, custom) in module.custom.iter_mut().enumerate() {
        if custom.name != "__wasm_bindgen_unstable" {
            continue;
        }
        to_remove.push(i);
        program_storage.push(mem::replace(&mut custom.value, Vec::new()));
    }

    for i in to_remove.into_iter().rev() {
        module.custom.remove(i);
    }

    let mut ret = Vec::new();
    for program in program_storage.iter() {
        let mut payload = &program[..];
        while let Some(data) = get_remaining(&mut payload) {
            // Historical versions of wasm-bindgen have used JSON as the custom
            // data section format. Newer versions, however, are using a custom
            // serialization protocol that looks much more like the wasm spec.
            //
            // We, however, want a sanity check to ensure that if we're running
            // against the wrong wasm-bindgen we get a nicer error than an
            // internal decode error. To that end we continue to verify a tiny
            // bit of json at the beginning of each blob before moving to the
            // next blob. This should keep us compatible with older wasm-bindgen
            // instances as well as forward-compatible for now.
            //
            // Note, though, that as `wasm-pack` picks up steam it's hoped we
            // can just delete this entirely. The `wasm-pack` project already
            // manages versions for us, so we in theory should need this check
            // less and less over time.
            if let Some(their_version) = verify_schema_matches(data)? {
                bail!(
                    "

it looks like the Rust project used to create this wasm file was linked against
a different version of wasm-bindgen than this binary:

  rust wasm file: {}
     this binary: {}

Currently the bindgen format is unstable enough that these two version must
exactly match, so it's required that these two version are kept in sync by
either updating the wasm-bindgen dependency or this binary. You should be able
to update the wasm-bindgen dependency with:

    cargo update -p wasm-bindgen

or you can update the binary with

    cargo install -f wasm-bindgen-cli

if this warning fails to go away though and you're not sure what to do feel free
to open an issue at https://github.com/rustwasm/wasm-bindgen/issues!
",
                    their_version,
                    my_version,
                );
            }
            let next = get_remaining(&mut payload).unwrap();
            ret.push(<decode::Program as decode::Decode>::decode_all(next));
        }
    }
    Ok(ret)
}

fn get_remaining<'a>(data: &mut &'a [u8]) -> Option<&'a [u8]> {
    if data.len() == 0 {
        return None;
    }
    let len = ((data[0] as usize) << 0)
        | ((data[1] as usize) << 8)
        | ((data[2] as usize) << 16)
        | ((data[3] as usize) << 24);
    let (a, b) = data[4..].split_at(len);
    *data = b;
    Some(a)
}

fn verify_schema_matches<'a>(data: &'a [u8]) -> Result<Option<&'a str>, Error> {
    macro_rules! bad {
        () => {
            bail!("failed to decode what looked like wasm-bindgen data")
        };
    }
    let data = match str::from_utf8(data) {
        Ok(s) => s,
        Err(_) => bad!(),
    };
    if !data.starts_with("{") || !data.ends_with("}") {
        bad!()
    }
    let needle = "\"schema_version\":\"";
    let rest = match data.find(needle) {
        Some(i) => &data[i + needle.len()..],
        None => bad!(),
    };
    let their_schema_version = match rest.find("\"") {
        Some(i) => &rest[..i],
        None => bad!(),
    };
    if their_schema_version == wasm_bindgen_shared::SCHEMA_VERSION {
        return Ok(None);
    }
    let needle = "\"version\":\"";
    let rest = match data.find(needle) {
        Some(i) => &data[i + needle.len()..],
        None => bad!(),
    };
    let their_version = match rest.find("\"") {
        Some(i) => &rest[..i],
        None => bad!(),
    };
    Ok(Some(their_version))
}

fn reset_indentation(s: &str) -> String {
    let mut indent: u32 = 0;
    let mut dst = String::new();

    for line in s.lines() {
        let line = line.trim();
        if line.starts_with('}') || (line.ends_with('}') && !line.starts_with('*')) {
            indent = indent.saturating_sub(1);
        }
        let extra = if line.starts_with(':') || line.starts_with('?') {
            1
        } else {
            0
        };
        if !line.is_empty() {
            for _ in 0..indent + extra {
                dst.push_str("    ");
            }
            dst.push_str(line);
        }
        dst.push_str("\n");
        if line.ends_with('{') {
            indent += 1;
        }
    }
    return dst;
}

// Eventually these will all be CLI options, but while they're unstable features
// they're left as environment variables. We don't guarantee anything about
// backwards-compatibility with these options.
fn threads_config() -> Option<wasm_bindgen_threads_xform::Config> {
    if env::var("WASM_BINDGEN_THREADS").is_err() {
        return None;
    }
    let mut cfg = wasm_bindgen_threads_xform::Config::new();
    if let Ok(s) = env::var("WASM_BINDGEN_THREADS_MAX_MEMORY") {
        cfg.maximum_memory(s.parse().unwrap());
    }
    if let Ok(s) = env::var("WASM_BINDGEN_THREADS_STACK_SIZE") {
        cfg.thread_stack_size(s.parse().unwrap());
    }
    Some(cfg)
}

fn demangle(module: &mut Module) {
    for func in module.funcs.iter_mut() {
        let name = match &func.name {
            Some(name) => name,
            None => continue,
        };
        if let Ok(sym) = rustc_demangle::try_demangle(name) {
            func.name = Some(sym.to_string());
        }
    }
}
