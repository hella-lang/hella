//! Phase 2 codegen — LLVM via `inkwell` 0.10 (llvm21-1).
//! All locals/params are `alloca` in entry block; structs lowered to llvm.struct with GEP.
//!
//! Async (Async-6/Async-7): `async` functions run on worker threads through
//! the `hella_async` runtime (compiled from `runtime/hella_async.c` only
//! when the program uses async — see Async-8 in the CLI). A `task<T>` value
//! is an opaque `hella_task_t*` handle: `spawn`/async-call allocates it,
//! `await` block-joins it and loads the inline result payload.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::Path;

use inkwell::IntPredicate;
use inkwell::builder::Builder;
use inkwell::context::Context;
use inkwell::module::Module;
use inkwell::types::{BasicType, BasicTypeEnum, StructType};
use inkwell::values::{BasicValue, BasicValueEnum, FunctionValue, PointerValue};

use crate::ast::*;
use crate::token::Span;

#[derive(Debug)]
pub struct CodegenError {
    pub message: String,
    pub span: Span,
}

#[derive(Clone)]
struct LoopContext<'ctx> {
    cond_bb: inkwell::basic_block::BasicBlock<'ctx>,
    exit_bb: inkwell::basic_block::BasicBlock<'ctx>,
    label: Option<String>,
    defer_depth: usize,
}

pub struct Codegen<'ctx> {
    context: &'ctx Context,
    module: Module<'ctx>,
    builder: Builder<'ctx>,
    vars: Vec<HashMap<String, (PointerValue<'ctx>, BasicTypeEnum<'ctx>)>>,
    globals: HashMap<String, (PointerValue<'ctx>, BasicTypeEnum<'ctx>)>,
    funcs: HashMap<String, (FunctionValue<'ctx>, TyInfo)>,
    struct_types: HashMap<String, StructType<'ctx>>,
    struct_fields: HashMap<String, HashMap<String, u32>>, // struct -> field -> index
    struct_field_defaults: HashMap<String, HashMap<String, Expr>>, // struct -> field -> default expr (if any)
    enum_types: HashMap<String, StructType<'ctx>>,
    enum_variant_tags: HashMap<String, HashMap<String, u32>>,
    /// Wide payload layout for enums with a multi-param variant:
    /// `{i32 tag, [WORDS x i64]}` plus per-variant field LLVM types.
    /// Enums without multi-param variants keep the legacy `{i32, i64}`.
    enum_wide_words: HashMap<String, u32>,
    enum_payload_tys: HashMap<String, HashMap<String, Vec<BasicTypeEnum<'ctx>>>>,
    class_methods: HashMap<String, HashMap<String, (FunctionValue<'ctx>, TyInfo)>>,
    class_constructors: HashMap<String, Vec<(FunctionValue<'ctx>, TyInfo)>>,
    class_destructors: HashMap<String, Vec<(FunctionValue<'ctx>, TyInfo)>>,
    class_properties: HashMap<String, HashMap<String, PropertyCG<'ctx>>>,
    class_operators: HashMap<String, HashMap<String, (FunctionValue<'ctx>, TyInfo)>>,
    loop_stack: Vec<LoopContext<'ctx>>,
    defer_stack: Vec<Vec<DeferStmt>>,
    /// Locals requiring destructor calls at scope exit, in declaration order.
    /// Parallel to `defer_stack`: pushed/popped together with each
    /// `codegen_block` scope (plus the manual `for`-var scope). Each entry
    /// is `(alloca, class_name)`.
    scope_dtors: Vec<Vec<(PointerValue<'ctx>, String)>>,
    /// Owned heap slots per scope: `(alloca of pair, inner type name)`.
    /// Destroyed (dtor + free) at scope exit with a null-data guard.
    own_slots: Vec<Vec<(PointerValue<'ctx>, String)>>,
    cur_fn: Option<FunctionValue<'ctx>>,
    cur_is_main: bool,
    cur_class: Option<String>,
    closure_count: usize,
    /// main's `args` array alloca plus the hidden `__hella_argc` alloca
    /// holding the true argument count (argv[1..] length, capped at 16).
    /// `args.len()`/`is_empty()`/`contains`/`first`/`last` and `for..in`
    /// observe argc through these instead of the static 16 slots. Matched
    /// by alloca pointer (never by name), so shadowing inside main is safe.
    /// Set in the main-with-args prologue, reset on every function entry.
    main_args_alloca: Option<PointerValue<'ctx>>,
    main_argc_alloca: Option<PointerValue<'ctx>>,
    /// Variables holding vectors (`TYPE vec` or `any x = vec[]`). Their LLVM
    /// type is the vec struct `{ [16 x E], i64 len }`; this set distinguishes
    /// them from class instances (also structs) for `push`/index/`for`.
    vec_vars: HashSet<String>,
    /// Variables holding maps (`K:V` or `any m = has ... end`). LLVM type is
    /// the map struct `{ [16 x K], [16 x V], i64 len }`.
    map_vars: HashSet<String>,
    /// Variables holding `task<T>` handles (Async-6). LLVM type is an
    /// opaque ptr; this map carries the sema `task<T>` for `await` result
    /// typing.
    task_vars: HashMap<String, crate::sema::Ty>,
    /// Variables holding strings (`string s = ...`). LLVM type is `ptr`;
    /// tracked so `len()`/`is_empty()` lower instead of falling through to
    /// class-method resolution.
    string_vars: HashSet<String>,
    /// Variables holding unsigned ints (`u8..u128`, `uint`). LLVM ints
    /// carry no signedness, so `infer_expr_ty` reports every int-typed
    /// local as `Ty::Int`; without this set `>>` on a `u64` local would
    /// lower to an arithmetic (sign-propagating) shift. Tracked at
    /// declaration like `string_vars` so `>>` lowers to a logical
    /// (zero-fill) shift for unsigned operands.
    unsigned_vars: HashSet<String>,
    /// Extern functions declared with a Hella `int` return that lower to a
    /// true C `int` (i32). Call results are sign-extended to Hella `int`
    /// (i64) at the call site — zero-extension would destroy the sign of
    /// e.g. `strcmp` (see `compare`), which only surfaced on libc
    /// implementations that don't return full-width negatives.
    extern_int32_rets: HashSet<String>,
    /// Extern functions declared with an unsigned 32-bit return (`u32`)
    /// that lower to C `uint32_t` (i32). Call results are zero-extended to
    /// Hella width at the call site (sext would corrupt values >= 2^31).
    extern_uint32_rets: HashSet<String>,
    /// Trait names declared in the program (for trait-object lowering).
    trait_names: HashSet<String>,
    /// `{data ptr, type tag}` pair struct type per named type that can
    /// back an `own` slot or trait object (traits, classes, structs),
    /// created in the declare phase.
    pair_types: HashMap<String, StructType<'ctx>>,
    /// Dynamic-type tag per class (stable within a build).
    class_tags: HashMap<String, u64>,
    next_class_tag: u64,
    /// Direct `extends` parent per class (for transitive implementors).
    class_extends: HashMap<String, String>,
    /// Directly implemented traits per class.
    class_implements: HashMap<String, Vec<String>>,
    /// Generated per-element-type destructor for vectors/maps holding
    /// owned content: `__container_dtor_N(slot)`. Memoized; emitted on
    /// demand, called from scope-exit, assignment-overwrite, and `clear`.
    container_dtors: HashMap<String, FunctionValue<'ctx>>,
    /// Release mode (`hella build --release`): `debug_assert` is stripped
    /// (not emitted; sema still checks it). Set from `OptLevel` before
    /// `compile_program`.
    pub release: bool,
    /// Global slots needing destruction at program end (`main` exit):
    /// `own` pairs plus structs with user dtors or transitive `own`
    /// fields. Emitted in reverse declaration order.
    global_owns: Vec<(PointerValue<'ctx>, String)>,
    global_dtors: Vec<GlobalDtor<'ctx>>,
    /// Global initializers too complex to const-fold, evaluated at program
    /// start (in `main`, after the user `init` block): `(name, init)`.
    pending_global_inits: Vec<(String, Expr)>,
}

/// One program-end destruction entry: a single slot, or a fixed array slot
/// expanded per static index at emission (the builder may not exist at
/// declaration time, so indices materialize late).
#[derive(Clone, Debug)]
enum GlobalDtor<'ctx> {
    One(PointerValue<'ctx>, String),
    Array {
        slot: PointerValue<'ctx>,
        elem_ty: BasicTypeEnum<'ctx>,
        len: u32,
    },
}

#[derive(Clone, Debug)]
struct PropertyCG<'ctx> {
    ty: crate::sema::Ty,
    getter: Option<(FunctionValue<'ctx>, TyInfo)>,
    setter: Option<(FunctionValue<'ctx>, TyInfo)>,
}

#[derive(Clone, Debug)]
struct TyInfo {
    ret: crate::sema::Ty,
    params: Vec<crate::sema::Ty>,
    param_modes: Vec<ParamMode>,
    param_names: Vec<String>,
    param_is_variadic: Vec<bool>,
    /// Default value per parameter (`None` = required); leading entry is
    /// always `None` for the implicit `this`. Filled at call sites.
    param_defaults: Vec<Option<crate::ast::Expr>>,
    /// `true` for `async` functions (Async-6): the declared LLVM function
    /// keeps the SYNC signature (params -> Ret) and runs the body inline;
    /// every call site spawns it on a worker thread instead and gets a
    /// `task<Ret>` handle back. Stored so call codegen can distinguish
    /// "call directly" (sync) from "spawn" (async).
    is_async: bool,
}

impl<'ctx> Codegen<'ctx> {
    pub fn new(context: &'ctx Context, module_name: &str) -> Self {
        let module = context.create_module(module_name);
        let builder = context.create_builder();
        Self {
            context,
            module,
            builder,
            vars: Vec::new(),
            globals: HashMap::new(),
            struct_field_defaults: HashMap::new(),
            funcs: HashMap::new(),
            struct_types: HashMap::new(),
            struct_fields: HashMap::new(),
            enum_types: HashMap::new(),
            enum_variant_tags: HashMap::new(),
            enum_wide_words: HashMap::new(),
            enum_payload_tys: HashMap::new(),
            class_methods: HashMap::new(),
            class_constructors: HashMap::new(),
            class_destructors: HashMap::new(),
            class_properties: HashMap::new(),
            class_operators: HashMap::new(),
            closure_count: 0,
            loop_stack: Vec::new(),
            defer_stack: Vec::new(),
            scope_dtors: Vec::new(),
            own_slots: Vec::new(),
            cur_fn: None,
            cur_is_main: false,
            cur_class: None,
            main_args_alloca: None,
            main_argc_alloca: None,
            vec_vars: HashSet::new(),
            map_vars: HashSet::new(),
            string_vars: HashSet::new(),
            unsigned_vars: HashSet::new(),
            task_vars: HashMap::new(),
            extern_int32_rets: HashSet::new(),
            extern_uint32_rets: HashSet::new(),
            trait_names: HashSet::new(),
            pair_types: HashMap::new(),
            class_tags: HashMap::new(),
            next_class_tag: 1,
            class_extends: HashMap::new(),
            class_implements: HashMap::new(),
            container_dtors: HashMap::new(),
            release: false,
            global_owns: Vec::new(),
            global_dtors: Vec::new(),
            pending_global_inits: Vec::new(),
        }
    }

    pub fn get_module_ir(&self) -> String {
        // NOTE: never `print_to_string` here. It returns an LLVMString
        // whose drop calls LLVMDisposeMessage, which segfaults
        // (STATUS_ACCESS_VIOLATION) on Windows — upstream Windows LLVM
        // builds use rpmalloc and the free crosses heaps. Round-trip
        // through a temp file instead: `print_to_file` creates no
        // LLVMString on success.
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "hella-ir-{}-{}.ll",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
        self.module
            .print_to_file(&path)
            .expect("failed to write module IR to temp file");
        let ir =
            std::fs::read_to_string(&path).expect("failed to read module IR back");
        let _ = std::fs::remove_file(&path);
        ir
    }

    /// Run the standard O3 pipeline over the module via the new pass
    /// manager (`hella build --release`). Call once, after
    /// `compile_program` + `verify`, before object emission / IR dump.
    /// Needs the target machine so passes can query target specifics.
    pub fn optimize_for_release(
        &self,
        machine: &inkwell::targets::TargetMachine,
    ) -> Result<(), String> {
        let options = inkwell::passes::PassBuilderOptions::create();
        self.module
            .run_passes("default<O3>", machine, options)
            .map_err(|e| e.to_string())
    }

    pub fn compile_program(
        &mut self,
        prog: &Program,
    ) -> Result<(), CodegenError> {
        // Async-8: compute which declarations the C runtime can actually
        // reach. Async functions outside that set are never emitted, so a
        // program that merely *declares* (or imports) async code does not
        // reference the async runtime and must not link it.
        let reach = crate::async_req::analyze(prog);
        let skip_fn = |f: &Function| -> bool {
            f.is_async && f.name != "main" && !reach.reachable.contains(&f.name)
        };
        for item in &prog.items {
            let it: &Item = match item {
                Item::Attributed{attrs: _, item} => item.as_ref(),
                other => other,
            };
            match it {
                Item::Struct(s) => self.declare_struct(s)?,
                Item::Class(c) => self.declare_class(c)?,
                Item::Trait(t) => self.declare_trait(t),
                Item::Enum(e) => self.declare_enum(e)?,
                Item::Typedef(td) => self.declare_typedef(td)?,
                Item::Distinct(dd) => self.declare_distinct(dd)?,
                Item::Extension(ext) => self.declare_extension(ext)?,
                Item::Extern(ext) => self.declare_extern(ext)?,
                Item::Const(c) => self.declare_const(c)?,
                Item::Var(v) => self.declare_global_var(v)?,
                _ => {}
            }
        }
        for item in &prog.items {
            let it: &Item = match item {
                Item::Attributed{attrs: _, item} => item.as_ref(),
                other => other,
            };
            if let Item::Function(f) = it {
                if skip_fn(f) { continue; }
                self.declare_function(f)?;
            }
        }
        // Dynamic-type tags for trait objects (needs all classes declared).
        self.assign_class_tags();
        for item in &prog.items {
            let it: &Item = match item {
                Item::Attributed{attrs: _, item} => item.as_ref(),
                other => other,
            };
            match it {
                Item::Function(f) => {
                    if skip_fn(f) { continue; }
                    self.codegen_function(f)?
                }
                Item::Class(c) => {
                    for m in &c.methods { self.codegen_class_method(c, m)?; }
                    for (idx, ctor) in c.constructors.iter().enumerate() { self.codegen_constructor(c, ctor, idx)?; }
                    for (idx, dtor) in c.destructors.iter().enumerate() { self.codegen_destructor(c, dtor, idx)?; }
                    for prop in &c.properties { self.codegen_property(c, prop)?; }
                    for op in &c.operators { self.codegen_operator(c, op)?; }
                    for conv in &c.conversions { self.codegen_conversion(c, conv)?; }
                }
                Item::Extension(ext) => self.codegen_extension(ext)?,
                Item::Init(blk) => self.codegen_init(blk)?,
                Item::Extern(_) => {}, // already declared
                _ => {}
            }
        }
        // NOTE: no end-of-compile `module.verify()` here. Every construct
        // is verified at its own lowering site (`func.verify(true)` on
        // extensions, ctors, dtors, properties, operators, conversions),
        // and `compile_to_object` verifies again before emission, so this
        // would be redundant — and `LLVMVerifyModule` segfaults
        // (STATUS_ACCESS_VIOLATION) on Windows for ordinary modules while
        // the function-level checks pass. If that upstream issue is ever
        // fixed, the check can come back as defense in depth.
        Ok(())
    }

    /// Assign dynamic-type tags to every class implementing ≥1 trait
    /// (directly or via `extends`), in sorted order for deterministic
    /// modules. Runs once after the declare phase, before any body
    /// codegen, so forward references resolve.
    /// Assign a dynamic-type tag to every declared class (sorted, so
    /// modules are deterministic). Tags back `own` pairs and trait
    /// dispatch; classes never flow into either path simply never use
    /// their tag.
    fn assign_class_tags(&mut self) {
        let mut names: Vec<String> = self.class_methods.keys().cloned().collect();
        names.sort();
        for name in names {
            if self.class_tags.contains_key(&name) {
                continue;
            }
            let t = self.next_class_tag;
            self.next_class_tag += 1;
            self.class_tags.insert(name, t);
        }
    }

    /// True when `class` implements any known trait (directly or through
    /// its `extends` chain; cycle-guarded).
    fn class_transitively_implements_any(&self, class: &str) -> bool {
        let mut seen = HashSet::new();
        let mut cur = Some(class.to_string());
        while let Some(name) = cur {
            if !seen.insert(name.clone()) {
                break;
            }
            if let Some(impls) = self.class_implements.get(&name) {
                if impls.iter().any(|t| self.trait_names.contains(t)) {
                    return true;
                }
            }
            cur = self.class_extends.get(&name).cloned();
        }
        false
    }

    /// All classes implementing `trait_name` (directly or via `extends`),
    /// sorted for deterministic dispatch. Cycle-guarded.
    fn implementors_of(&self, trait_name: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .class_methods
            .keys()
            .filter(|c| {
                let mut seen = HashSet::new();
                let mut cur = Some((*c).clone());
                while let Some(name) = cur {
                    if !seen.insert(name.clone()) {
                        break;
                    }
                    if let Some(impls) = self.class_implements.get(&name) {
                        if impls.iter().any(|t| t == trait_name) {
                            return true;
                        }
                    }
                    cur = self.class_extends.get(&name).cloned();
                }
                false
            })
            .cloned()
            .collect();
        out.sort();
        out
    }

    /// Declared function for `method` on a concrete class (includes
    /// inherited parent methods, merged by `declare_class`).
    fn method_func_of(
        &self,
        class: &str,
        method: &str,
    ) -> Option<(FunctionValue<'ctx>, TyInfo)> {
        self.class_methods
            .get(class)?
            .get(method)
            .cloned()
    }

    /// Which named type (if any) owns this pair struct type?
    fn pair_owner_of(&self, st: StructType<'ctx>) -> Option<String> {
        self.pair_types
            .iter()
            .find(|(_, v)| **v == st)
            .map(|(k, _)| k.clone())
    }

    /// Pair struct type for an `own` inner type. Panics on invalid inner
    /// types (sema validates; mirrors neighboring `unwrap()`s).
    fn own_pair_type(&self, inner: &crate::sema::Ty) -> StructType<'ctx> {
        match inner {
            crate::sema::Ty::Struct(n) => {
                let lookup = n.rsplit("::").next().unwrap_or(n);
                *self
                    .pair_types
                    .get(lookup)
                    .unwrap_or_else(|| panic!("no pair type for {n}"))
            }
            _ => panic!("own requires a class, struct, or trait type"),
        }
    }

    /// Pair type when `name` is a TRAIT (classes/structs have pair types
    /// too, but lower to their data structs unless trait-dispatched).
    fn trait_pair_of(&self, name: &str) -> Option<StructType<'ctx>> {
        if self.trait_names.contains(name) {
            self.pair_types.get(name).cloned()
        } else {
            None
        }
    }

    /// Size in bytes of a struct type via GEP on null.
    fn struct_byte_size(&self, st: StructType<'ctx>) -> inkwell::values::IntValue<'ctx> {
        let ptr_ty = st.ptr_type(inkwell::AddressSpace::default());
        let null = ptr_ty.const_null();
        let gep = unsafe {
            self.builder
                .build_gep(st, null, &[self.context.i32_type().const_int(1, false)], "sizeof.gep")
                .unwrap()
        };
        self.builder
            .build_ptr_to_int(gep, self.context.i64_type(), "sizeof")
            .unwrap()
    }

    /// Pre-create the `{data ptr, type tag}` pair type for a named type
    /// (trait, class, or struct). Backs every `own` slot and trait object
    /// of that type.
    fn declare_pair_type(&mut self, name: &str) {
        if self.pair_types.contains_key(name) {
            return;
        }
        let pair = self
            .context
            .opaque_struct_type(&format!("__pair_{name}"));
        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
        pair.set_body(&[ptr_ty.into(), self.context.i64_type().into()], false);
        self.pair_types.insert(name.to_string(), pair);
    }

    /// Box a class-typed VALUE into a trait-pair value for a trait-typed
    /// destination: materialize it into a hidden slot and build `{slot ptr,
    /// tag}`. Everything else passes through untouched (callers keep their
    /// own coercion). A trait value flowing into `any` (opaque pointer)
    /// erases to its data pointer.
    fn box_trait_value(
        &self,
        val: BasicValueEnum<'ctx>,
        dest: BasicTypeEnum<'ctx>,
        span: Span,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        if val.get_type() == dest {
            return Ok(val);
        }
        // Erasure into `any`/opaque pointer: keep the data pointer.
        if let BasicTypeEnum::PointerType(_) = dest {
            if let BasicValueEnum::StructValue(sv) = val {
                if self.pair_owner_of(sv.get_type()).is_some() {
                    let data = self
                        .builder
                        .build_extract_value(sv, 0, "trait.erase")
                        .unwrap()
                        .into_pointer_value();
                    return Ok(data.into());
                }
            }
        }
        let BasicTypeEnum::StructType(dest_st) = dest else {
            return Ok(val);
        };
        let Some(trait_name) = self.pair_owner_of(dest_st) else {
            return Ok(val);
        };
        let BasicValueEnum::StructValue(val_st) = val else {
            return Err(CodegenError {
                message: format!("cannot convert value to trait `{trait_name}`"),
                span,
            });
        };
        if let Some(src_owner) = self.pair_owner_of(val_st.get_type()) {
            // Own-to-own trait upcast: allow when src class implements dest trait,
            // otherwise still copy data/tag (sema already validated).
            let data = self.builder.build_extract_value(val_st, 0, "own.convert.data").unwrap().into_pointer_value();
            let tag = self.builder.build_extract_value(val_st, 1, "own.convert.tag").unwrap();
            let mut new_pair: BasicValueEnum<'ctx> = dest_st.const_zero().into();
            let tmp = self.builder.build_insert_value(new_pair.into_struct_value(), data.as_basic_value_enum(), 0, "own.convert.data").unwrap();
            new_pair = tmp.as_basic_value_enum();
            let tmp2 = self.builder.build_insert_value(new_pair.into_struct_value(), tag, 1, "own.convert.tag").unwrap();
            new_pair = tmp2.as_basic_value_enum();
            return Ok(new_pair);
        }
        let class_name = self.ty_to_struct_name(&val.get_type())?;
        if !self.class_transitively_implements_any(&class_name)
            || !self.implementors_of(&trait_name).contains(&class_name)
        {
            return Err(CodegenError {
                message: format!("`{class_name}` does not implement trait `{trait_name}`"),
                span,
            });
        }
        let tag = *self.class_tags.get(&class_name).ok_or(CodegenError {
            message: format!("no dynamic tag for `{class_name}`"),
            span,
        })?;
        let class_st = *self.struct_types.get(&class_name).ok_or(CodegenError {
            message: format!("unknown class `{class_name}`"),
            span,
        })?;
        let slot = self
            .builder
            .build_alloca(class_st.as_basic_type_enum(), "trait.box")
            .unwrap();
        self.builder.build_store(slot, val).unwrap();
        let pair_slot = self.builder.build_alloca(dest, "trait.pair").unwrap();
        let data_ptr = self
            .builder
            .build_struct_gep(dest_st, pair_slot, 0, "trait.data.ptr")
            .unwrap();
        self.builder.build_store(data_ptr, slot).unwrap();
        let tag_ptr = self
            .builder
            .build_struct_gep(dest_st, pair_slot, 1, "trait.tag.ptr")
            .unwrap();
        self.builder.build_store(
            tag_ptr,
            self.context.i64_type().const_int(tag, false),
        )
        .unwrap();
        Ok(self.builder.build_load(dest, pair_slot, "trait.pair").unwrap())
    }

    /// Record a trait for trait-object lowering: its `{data ptr, type tag}`
    /// pair type is pre-created so value lowering stays `&self`.
    fn declare_trait(&mut self, t: &TraitDecl) {
        self.trait_names.insert(t.name.clone());
        self.declare_pair_type(&t.name);
    }

    fn declare_struct(&mut self, s: &StructDecl) -> Result<(), CodegenError> {
        if self.struct_types.contains_key(&s.name) {
            return Err(CodegenError {
                message: format!("duplicate struct {}", s.name),
                span: s.name_span,
            });
        }
        let opaque = self.context.opaque_struct_type(&s.name);
        // Insert early to allow self-reference (not needed Phase 2) and duplicate check
        self.struct_types.insert(s.name.clone(), opaque);
        self.declare_pair_type(&s.name);
        // Collect field LLVM types
        let mut field_map = HashMap::new();
        let mut field_tys: Vec<BasicTypeEnum<'ctx>> = Vec::new();
        let mut field_defaults = HashMap::new();
        for (idx, f) in s.fields.iter().enumerate() {
            let lty = self.llvm_ty_for(&f.ty);
            field_map.insert(f.name.clone(), idx as u32);
            field_tys.push(lty);
            if let Some(def) = &f.default {
                field_defaults.insert(f.name.clone(), def.clone());
            }
        }
        opaque.set_body(&field_tys, false);
        self.struct_fields.insert(s.name.clone(), field_map);
        self.struct_field_defaults.insert(s.name.clone(), field_defaults);
        Ok(())
    }

    fn declare_class(&mut self, c: &ClassDecl) -> Result<(), CodegenError> {
        if self.struct_types.contains_key(&c.name) {
            return Err(CodegenError{message: format!("duplicate class/struct `{}`", c.name), span: c.name_span});
        }
        let opaque = self.context.opaque_struct_type(&c.name);
        self.struct_types.insert(c.name.clone(), opaque);
        self.declare_pair_type(&c.name);
        let mut field_map = HashMap::new();
        let mut field_tys = Vec::new();
        // For extends: prepend parent fields if parent already declared (otherwise defer)
        if let Some(ref parent_ty) = c.extends {
            if let Type::Named(pname, _) = parent_ty {
                if let Some(parent_struct) = self.struct_types.get(pname).cloned() {
                    if let Some(parent_fields) = self.struct_fields.get(pname).cloned() {
                        // parent fields already in struct_fields, copy layout
                        let parent_field_count = parent_struct.count_fields() as usize;
                        // Need to get field types from parent struct: use parent_struct.get_field_types
                        // For now, just copy from parent via iterating field_map order - simpler to reconstruct from parent_fields map sorted by index
                        let mut sorted: Vec<(String, u32)> = parent_fields.into_iter().map(|(k,v)| (k,v)).collect();
                        sorted.sort_by_key(|(_, idx)| *idx);
                        for (fname, idx) in sorted {
                            let fty = parent_struct.get_field_type_at_index(idx).unwrap();
                            field_map.insert(fname.clone(), field_tys.len() as u32);
                            field_tys.push(fty);
                        }
                        // Note: if parent not yet declared, skip merging (forward ref) - layout will be incomplete but ok for now
                    }
                }
            }
        }
        let mut field_defaults = HashMap::new();
        // For extends, also copy parent defaults if any
        if let Some(ref parent_ty) = c.extends {
            if let Type::Named(pname, _) = parent_ty {
                if let Some(parent_defaults) = self.struct_field_defaults.get(pname).cloned() {
                    for (k, v) in parent_defaults {
                        field_defaults.insert(k, v);
                    }
                }
            }
        }
        for f in c.fields.iter() {
            let lty = self.llvm_ty_for(&f.ty);
            field_map.insert(f.name.clone(), field_tys.len() as u32);
            field_tys.push(lty);
            if let Some(def) = &f.default {
                field_defaults.insert(f.name.clone(), def.clone());
            }
        }
        opaque.set_body(&field_tys, false);
        self.struct_fields.insert(c.name.clone(), field_map);
        self.struct_field_defaults.insert(c.name.clone(), field_defaults);
        // Record heritage for trait-object dispatch (transitive implementors
        // are computed on demand from these direct edges).
        if let Some(ref parent_ty) = c.extends {
            if let Type::Named(pname, _) = parent_ty {
                self.class_extends.insert(c.name.clone(), pname.clone());
            }
        }
        let mut direct_impls = Vec::new();
        for imp in &c.implements {
            if let Type::Named(n, _) = imp {
                direct_impls.push(n.clone());
            }
        }
        if !direct_impls.is_empty() {
            self.class_implements.insert(c.name.clone(), direct_impls);
        }
        // Declare methods
        let mut methods = HashMap::new();
        for m in &c.methods {
            let ret_ty_raw: crate::sema::Ty = (&m.ret_ty).into();
            let ret_ty = self.resolve_ty_for_codegen(&ret_ty_raw);
            let mut param_semas: Vec<crate::sema::Ty> = Vec::new();
            param_semas.push(crate::sema::Ty::Struct(c.name.clone()));
            for (idx, p) in m.params.iter().enumerate() {
                let raw: crate::sema::Ty = (&p.ty).into();
                let resolved = self.resolve_ty_for_codegen(&raw);
                let final_ty = if p.is_variadic {
                    if p.ty.name() == "__derived__" {
                        if idx == 0 {
                            crate::sema::Ty::Array(Box::new(crate::sema::Ty::Int))
                        } else {
                            let prev_raw: crate::sema::Ty = (&m.params[idx-1].ty).into();
                            let prev_res = self.resolve_ty_for_codegen(&prev_raw);
                            crate::sema::Ty::Array(Box::new(prev_res))
                        }
                    } else {
                        crate::sema::Ty::Array(Box::new(resolved))
                    }
                } else {
                    resolved
                };
                param_semas.push(final_ty);
            }
            let this_ty = self.context.ptr_type(inkwell::AddressSpace::default()).into();
            let mut param_llvm: Vec<inkwell::types::BasicMetadataTypeEnum> = vec![this_ty];
            for (idx, p) in m.params.iter().enumerate() {
                let t: crate::sema::Ty = (&p.ty).into();
                let sema_t = if p.is_variadic {
                    if p.ty.name() == "__derived__" {
                        if idx == 0 {
                            crate::sema::Ty::Array(Box::new(crate::sema::Ty::Int))
                        } else {
                            let prev_raw: crate::sema::Ty = (&m.params[idx-1].ty).into();
                            let prev_res = self.resolve_ty_for_codegen(&prev_raw);
                            crate::sema::Ty::Array(Box::new(prev_res))
                        }
                    } else {
                        crate::sema::Ty::Array(Box::new(self.resolve_ty_for_codegen(&t)))
                    }
                } else {
                    self.resolve_ty_for_codegen(&t)
                };
                // `ref`/`out` params take opaque pointers (mirrors `declare_function`).
                if p.mode != ParamMode::None {
                    param_llvm.push(self.context.ptr_type(inkwell::AddressSpace::default()).into());
                } else if let Some(bt) = self.llvm_ty_for_sema(&sema_t) { param_llvm.push(bt.into()); }
            }
            let fn_ty = match ret_ty {
                crate::sema::Ty::Void => self.context.void_type().fn_type(&param_llvm, false),
                crate::sema::Ty::Int => self.context.i64_type().fn_type(&param_llvm, false),
                crate::sema::Ty::UInt => self.context.i64_type().fn_type(&param_llvm, false),
                crate::sema::Ty::SizedInt { bits, .. } => self.llvm_int_for_bits(bits).fn_type(&param_llvm, false),
                crate::sema::Ty::Bool => self.context.bool_type().fn_type(&param_llvm, false),
                crate::sema::Ty::Char => self.context.i32_type().fn_type(&param_llvm, false),
                crate::sema::Ty::String => self.context.ptr_type(inkwell::AddressSpace::default()).fn_type(&param_llvm, false),
                crate::sema::Ty::Struct(ref n) => {
                    if let Some(pair) = self.trait_pair_of(n) {
                        pair.fn_type(&param_llvm, false)
                    } else {
                        let st = self.struct_types.get(n).unwrap();
                        st.fn_type(&param_llvm, false)
                    }
                }
                crate::sema::Ty::Own(ref inner) => {
                    self.own_pair_type(inner).fn_type(&param_llvm, false)
                }
                // `task<T>` (Async-6): opaque runtime handle pointer.
                crate::sema::Ty::Task(_) => self.context.ptr_type(inkwell::AddressSpace::default()).fn_type(&param_llvm, false),
                crate::sema::Ty::Array(_) => self.context.i64_type().array_type(16).fn_type(&param_llvm, false),
                crate::sema::Ty::FixedArray { elem: ref elem, size: ref size } => {
                    let n = size.unwrap_or(16) as u32;
                    match self.llvm_ty_for_sema(elem.as_ref()) {
                        Some(BasicTypeEnum::IntType(it)) => it.array_type(n).fn_type(&param_llvm, false),
                        Some(BasicTypeEnum::FloatType(ft)) => ft.array_type(n).fn_type(&param_llvm, false),
                        Some(BasicTypeEnum::PointerType(pt)) => pt.array_type(n).fn_type(&param_llvm, false),
                        Some(BasicTypeEnum::StructType(st)) => st.array_type(n).fn_type(&param_llvm, false),
                        Some(BasicTypeEnum::ArrayType(at)) => at.array_type(n).fn_type(&param_llvm, false),
                        _ => self.context.i64_type().array_type(n).fn_type(&param_llvm, false),
                    }
                },
                crate::sema::Ty::Vec(ref elem) => {
                    let inner = match elem.as_ref() {
                        crate::sema::Ty::Any => self.context.i64_type().into(),
                        _ => self.llvm_ty_for_sema(elem).unwrap_or_else(|| self.context.i64_type().into()),
                    };
                    self.vec_struct_ty(inner).fn_type(&param_llvm, false)
                },
                crate::sema::Ty::Map { key: ref key, value: ref value } => {
                    let k = match key.as_ref() {
                        crate::sema::Ty::Any => self.context.i64_type().into(),
                        _ => self.llvm_ty_for_sema(key.as_ref()).unwrap_or_else(|| self.context.i64_type().into()),
                    };
                    let v = match value.as_ref() {
                        crate::sema::Ty::Any => self.context.ptr_type(inkwell::AddressSpace::default()).into(),
                        _ => self.llvm_ty_for_sema(value.as_ref()).unwrap_or_else(|| self.context.i64_type().into()),
                    };
                    self.map_struct_ty(k, v).fn_type(&param_llvm, false)
                },
                crate::sema::Ty::Pointer(_) => self.context.ptr_type(inkwell::AddressSpace::default()).fn_type(&param_llvm, false),
                crate::sema::Ty::Optional(ref el) => {
                    let inner = self.llvm_ty_for_sema(el).unwrap();
                    self.context.struct_type(&[inner.into(), self.context.bool_type().into()], false).fn_type(&param_llvm, false)
                }
                crate::sema::Ty::Enum(ref n) => {
                    let et = self.enum_types.get(n).unwrap();
                    et.fn_type(&param_llvm, false)
                }
                crate::sema::Ty::Float => self.context.f32_type().fn_type(&param_llvm, false),
                crate::sema::Ty::Double => self.context.f64_type().fn_type(&param_llvm, false),
                crate::sema::Ty::Generic(_, _) => self.context.ptr_type(inkwell::AddressSpace::default()).fn_type(&param_llvm, false),
                crate::sema::Ty::Tuple(ref tys) => self.tuple_struct_ty(tys).map(|st| st.fn_type(&param_llvm, false)).unwrap_or_else(|| self.context.ptr_type(inkwell::AddressSpace::default()).fn_type(&param_llvm, false)),
                crate::sema::Ty::Any => self.context.ptr_type(inkwell::AddressSpace::default()).fn_type(&param_llvm, false),
                crate::sema::Ty::Function(_, _) => self.context.ptr_type(inkwell::AddressSpace::default()).fn_type(&param_llvm, false),

            };
            let mangled = format!("{}__{}", c.name, m.name);
            let func = self.module.add_function(&mangled, fn_ty, None);
            let mut full_names = vec!["this".to_string()];
            full_names.extend(m.params.iter().map(|p| p.name.clone()));
            let mut full_modes = vec![ParamMode::None];
            full_modes.extend(m.params.iter().map(|p| p.mode));
            let mut full_variadic = vec![false];
            full_variadic.extend(m.params.iter().map(|p| p.is_variadic));
            let mut full_defaults = vec![None];
            full_defaults.extend(m.params.iter().map(|p| p.default.clone()));
            let tyinfo = TyInfo{ret: ret_ty.clone(), params: param_semas.clone(), param_modes: full_modes, param_names: full_names, param_is_variadic: full_variadic, param_defaults: full_defaults, is_async: false};
            methods.insert(m.name.clone(), (func, tyinfo));
        }
        self.class_methods.insert(c.name.clone(), methods);
        // Declare operators
        let mut ops: std::collections::HashMap<String, (FunctionValue<'ctx>, TyInfo)> = std::collections::HashMap::new();
        for op in &c.operators {
            let ret_ty = crate::sema::Ty::Int; // MVP: operators return int
            let mut param_semas = vec![crate::sema::Ty::Struct(c.name.clone())];
            for (idx, pp) in op.params.iter().enumerate() {
                let raw: crate::sema::Ty = (&pp.ty).into();
                let res = self.resolve_ty_for_codegen(&raw);
                let final_ty = if pp.is_variadic {
                    if pp.ty.name() == "__derived__" {
                        if idx == 0 { crate::sema::Ty::Array(Box::new(crate::sema::Ty::Int)) } else {
                            let prev_raw: crate::sema::Ty = (&op.params[idx-1].ty).into();
                            crate::sema::Ty::Array(Box::new(self.resolve_ty_for_codegen(&prev_raw)))
                        }
                    } else { crate::sema::Ty::Array(Box::new(res)) }
                } else { res };
                param_semas.push(final_ty);
            }
            let this_ty = self.context.ptr_type(inkwell::AddressSpace::default()).into();
            let mut param_llvm: Vec<inkwell::types::BasicMetadataTypeEnum> = vec![this_ty];
            for (idx, pp) in op.params.iter().enumerate() {
                let t: crate::sema::Ty = (&pp.ty).into();
                let sema_t = if pp.is_variadic {
                    if pp.ty.name() == "__derived__" {
                        if idx == 0 { crate::sema::Ty::Array(Box::new(crate::sema::Ty::Int)) } else {
                            let prev_raw: crate::sema::Ty = (&op.params[idx-1].ty).into();
                            crate::sema::Ty::Array(Box::new(self.resolve_ty_for_codegen(&prev_raw)))
                        }
                    } else { crate::sema::Ty::Array(Box::new(self.resolve_ty_for_codegen(&t))) }
                } else { self.resolve_ty_for_codegen(&t) };
                // `ref`/`out` params take opaque pointers (mirrors `declare_function`).
                if pp.mode != ParamMode::None {
                    param_llvm.push(self.context.ptr_type(inkwell::AddressSpace::default()).into());
                } else if let Some(bt) = self.llvm_ty_for_sema(&sema_t) { param_llvm.push(bt.into()); }
            }
            let fn_ty = match ret_ty {
                crate::sema::Ty::Int => self.context.i64_type().fn_type(&param_llvm, false),
                crate::sema::Ty::UInt => self.context.i64_type().fn_type(&param_llvm, false),
                crate::sema::Ty::SizedInt { bits, .. } => self.llvm_int_for_bits(bits).fn_type(&param_llvm, false),
                crate::sema::Ty::Bool => self.context.bool_type().fn_type(&param_llvm, false),
                _ => self.context.i64_type().fn_type(&param_llvm, false),
            };
            let op_mangled = match op.op.as_str() {
                "+" => "plus", "-" => "minus", "*" => "star", "/" => "slash", "%" => "percent",
                "<" => "lt", "<=" => "le", ">" => "gt", ">=" => "ge",
                "is" => "is", "is not" => "is_not",
                "&" => "bitand", "|" => "bitor", "^" => "xor", "~" => "tilde",
                "<<" => "lshift", ">>" => "rshift", "=" => "assign", "[]" => "index",
                "++" => "inc", "--" => "dec",
                "+=" => "plus_assign", "-=" => "minus_assign", "*=" => "star_assign", "/=" => "slash_assign", "%=" => "percent_assign",
                "&=" => "and_assign", "|=" => "or_assign", "^=" => "xor_assign", "<<=" => "lshift_assign", ">>=" => "rshift_assign",
                _ => "op",
            };
            let mangled = format!("{}__op_{}", c.name, op_mangled);
            let func = self.module.add_function(&mangled, fn_ty, None);
            let mut full_names = vec!["this".to_string()];
            full_names.extend(op.params.iter().map(|p| p.name.clone()));
            let mut full_modes = vec![ParamMode::None];
            full_modes.extend(op.params.iter().map(|p| p.mode));
            let mut full_variadic = vec![false];
            full_variadic.extend(op.params.iter().map(|p| p.is_variadic));
            let mut full_defaults = vec![None];
            full_defaults.extend(op.params.iter().map(|p| p.default.clone()));
            ops.insert(op.op.clone(), (func, TyInfo{ret: ret_ty.clone(), params: param_semas.clone(), param_modes: full_modes, param_names: full_names, param_is_variadic: full_variadic, param_defaults: full_defaults, is_async: false}));
        }
        if !ops.is_empty() { self.class_operators.insert(c.name.clone(), ops); }
        // Inherit parent methods for extends (static dispatch)
        if let Some(ref parent_ty) = c.extends {
            if let Type::Named(pname, _) = parent_ty {
                if let Some(parent_methods) = self.class_methods.get(pname).cloned() {
                    if let Some(child_methods) = self.class_methods.get_mut(&c.name) {
                        for (mname, sig) in parent_methods {
                            child_methods.entry(mname).or_insert(sig);
                        }
                    }
                }
            }
        }
        // Declare constructors
        let mut ctors = Vec::new();
        for (idx, ctor) in c.constructors.iter().enumerate() {
            let mut param_semas = vec![crate::sema::Ty::Struct(c.name.clone())];
            for (pidx, p) in ctor.params.iter().enumerate() {
                let raw: crate::sema::Ty = (&p.ty).into();
                let res = self.resolve_ty_for_codegen(&raw);
                let final_ty = if p.is_variadic {
                    if p.ty.name() == "__derived__" {
                        if pidx == 0 { crate::sema::Ty::Array(Box::new(crate::sema::Ty::Int)) } else {
                            let prev_raw: crate::sema::Ty = (&ctor.params[pidx-1].ty).into();
                            crate::sema::Ty::Array(Box::new(self.resolve_ty_for_codegen(&prev_raw)))
                        }
                    } else { crate::sema::Ty::Array(Box::new(res)) }
                } else { res };
                param_semas.push(final_ty);
            }
            let this_ty = self.context.ptr_type(inkwell::AddressSpace::default()).into();
            let mut param_llvm: Vec<inkwell::types::BasicMetadataTypeEnum> = vec![this_ty];
            for (pidx, p) in ctor.params.iter().enumerate() {
                let raw: crate::sema::Ty = (&p.ty).into();
                let sema_t = if p.is_variadic {
                    if p.ty.name() == "__derived__" {
                        if pidx == 0 { crate::sema::Ty::Array(Box::new(crate::sema::Ty::Int)) } else {
                            let prev_raw: crate::sema::Ty = (&ctor.params[pidx-1].ty).into();
                            crate::sema::Ty::Array(Box::new(self.resolve_ty_for_codegen(&prev_raw)))
                        }
                    } else {
                        let res = self.resolve_ty_for_codegen(&raw);
                        crate::sema::Ty::Array(Box::new(res))
                    }
                } else { self.resolve_ty_for_codegen(&raw) };
                // `ref`/`out` params take opaque pointers (mirrors `declare_function`).
                if p.mode != ParamMode::None {
                    param_llvm.push(self.context.ptr_type(inkwell::AddressSpace::default()).into());
                } else if let Some(bt) = self.llvm_ty_for_sema(&sema_t) { param_llvm.push(bt.into()); }
            }
            let fn_ty = self.context.void_type().fn_type(&param_llvm, false);
            let mangled = format!("{}__ctor{}", c.name, if c.constructors.len()>1 { format!("{}", idx)} else {"".to_string()});
            let func = self.module.add_function(&mangled, fn_ty, None);
            let mut full_names = vec!["this".to_string()];
            full_names.extend(ctor.params.iter().map(|p| p.name.clone()));
            let mut full_modes = vec![ParamMode::None];
            full_modes.extend(ctor.params.iter().map(|p| p.mode));
            let mut full_variadic = vec![false];
            full_variadic.extend(ctor.params.iter().map(|p| p.is_variadic));
            let mut full_defaults = vec![None];
            full_defaults.extend(ctor.params.iter().map(|p| p.default.clone()));
            ctors.push((func, TyInfo{ret: crate::sema::Ty::Void, params: param_semas.clone(), param_modes: full_modes, param_names: full_names, param_is_variadic: full_variadic, param_defaults: full_defaults, is_async: false}));
        }
        if !ctors.is_empty() { self.class_constructors.insert(c.name.clone(), ctors); }
        // Declare destructors: `void (ptr this)`, mangled `Class__dtor`
        let mut dtors = Vec::new();
        for (idx, _dtor) in c.destructors.iter().enumerate() {
            let this_ty: inkwell::types::BasicMetadataTypeEnum = self.context.ptr_type(inkwell::AddressSpace::default()).into();
            let fn_ty = self.context.void_type().fn_type(&[this_ty], false);
            let mangled = format!("{}__dtor{}", c.name, if c.destructors.len()>1 { format!("{}", idx)} else {"".to_string()});
            let func = self.module.add_function(&mangled, fn_ty, None);
            dtors.push((func, TyInfo{ret: crate::sema::Ty::Void, params: vec![crate::sema::Ty::Struct(c.name.clone())], param_modes: vec![ParamMode::None], param_names: vec!["this".to_string()], param_is_variadic: vec![false], param_defaults: vec![None], is_async: false}));
        }
        if !dtors.is_empty() { self.class_destructors.insert(c.name.clone(), dtors); }
        // Declare properties: getter/setter — allow separate declarations that merge
        let mut props = HashMap::new();
        for prop in &c.properties {
            let prop_ty_raw: crate::sema::Ty = prop.ty.as_ref().map(|t| t.into()).or_else(|| prop.setter.as_ref().map(|(p,_)| (&p.ty).into())).unwrap_or(crate::sema::Ty::Int);
            let prop_ty = self.resolve_ty_for_codegen(&prop_ty_raw);
            let mut pg = None;
            let mut ps = None;
            if prop.getter.is_some() {
                let ret_llvm = self.llvm_ty_for_sema(&prop_ty).unwrap();
                let this_ty = self.context.ptr_type(inkwell::AddressSpace::default()).into();
                let fn_ty = match prop_ty {
                    crate::sema::Ty::Void => self.context.void_type().fn_type(&[this_ty], false),
                    _ => ret_llvm.fn_type(&[this_ty], false),
                };
                let mangled = format!("{}__get_{}", c.name, prop.name);
                // Reuse existing getter if already declared via merging, otherwise create
                let func = if let Some(existing) = props.get(&prop.name).and_then(|pc: &PropertyCG| pc.getter.as_ref().map(|(f,_)| *f)) {
                    existing
                } else {
                    self.module.add_function(&mangled, fn_ty, None)
                };
                let mut params = vec![crate::sema::Ty::Struct(c.name.clone())];
                pg = Some((func, TyInfo{ret: prop_ty.clone(), params: params.clone(), param_modes: vec![ParamMode::None; params.len()], param_names: Vec::new(), param_is_variadic: Vec::new(), param_defaults: Vec::new(), is_async: false}));
            }
            if let Some((ref param,_)) = prop.setter {
                let setter_ty_raw: crate::sema::Ty = (&param.ty).into();
                let setter_ty = self.resolve_ty_for_codegen(&setter_ty_raw);
                let this_ty = self.context.ptr_type(inkwell::AddressSpace::default()).into();
                let val_llvm = self.llvm_ty_for_sema(&setter_ty).unwrap();
                let fn_ty = self.context.void_type().fn_type(&[this_ty, val_llvm.into()], false);
                let mangled = format!("{}__set_{}", c.name, prop.name);
                let func = if let Some(existing) = props.get(&prop.name).and_then(|pc| pc.setter.as_ref().map(|(f,_)| *f)) {
                    existing
                } else {
                    self.module.add_function(&mangled, fn_ty, None)
                };
                let mut params = vec![crate::sema::Ty::Struct(c.name.clone()), setter_ty.clone()];
                ps = Some((func, TyInfo{ret: crate::sema::Ty::Void, params: params.clone(), param_modes: vec![ParamMode::None; params.len()], param_names: Vec::new(), param_is_variadic: Vec::new(), param_defaults: Vec::new(), is_async: false}));
            }
            if let Some(existing) = props.get(&prop.name).cloned() {
                let mut merged_getter = existing.getter;
                let mut merged_setter = existing.setter;
                if pg.is_some() {
                    if merged_getter.is_some() {
                        // duplicate getter - keep existing, error will be in sema
                    } else { merged_getter = pg; }
                }
                if ps.is_some() {
                    if merged_setter.is_some() {
                    } else { merged_setter = ps; }
                }
                props.insert(prop.name.clone(), PropertyCG{ty: existing.ty.clone(), getter: merged_getter, setter: merged_setter});
            } else {
                props.insert(prop.name.clone(), PropertyCG{ty: prop_ty, getter: pg, setter: ps});
            }
        }
        if !props.is_empty() { self.class_properties.insert(c.name.clone(), props); }
        // Inherit parent properties
        if let Some(ref parent_ty) = c.extends {
            if let Type::Named(pname, _) = parent_ty {
                if let Some(parent_props) = self.class_properties.get(pname).cloned() {
                    // ensure child's map exists
                    let entry = self.class_properties.entry(c.name.clone()).or_insert_with(HashMap::new);
                    for (pname2, prop) in parent_props {
                        entry.entry(pname2).or_insert(prop);
                    }
                }
            }
        }
        Ok(())
    }

    fn declare_enum(&mut self, e: &EnumDecl) -> Result<(), CodegenError> {
        if self.enum_types.contains_key(&e.name) || self.struct_types.contains_key(&e.name) || self.class_methods.contains_key(&e.name) {
            return Err(CodegenError{message: format!("duplicate enum `{}`", e.name), span: e.name_span});
        }
        // Per-variant payload field types (fallible: unknown types error
        // instead of panicking on forward references).
        let mut payload_tys: HashMap<String, Vec<BasicTypeEnum<'ctx>>> = HashMap::new();
        let mut max_arity = 0usize;
        for v in &e.variants {
            let mut ftys = Vec::new();
            for p in &v.payload_params {
                let sty: crate::sema::Ty = (&p.ty).into();
                let resolved = self.resolve_ty_for_codegen(&sty);
                let Some(bt) = self.llvm_ty_for_sema(&resolved) else {
                    return Err(CodegenError{message: format!("unknown type `{}` for enum `{}` payload", p.ty.name(), e.name), span: p.span});
                };
                ftys.push(bt);
            }
            max_arity = max_arity.max(ftys.len());
            payload_tys.insert(v.name.clone(), ftys);
        }
        let enum_ty = self.context.opaque_struct_type(&e.name);
        if max_arity <= 1 {
            // Legacy layout: { i32 tag, i64 payload }.
            let payload_ty = self.context.i64_type();
            let tag_ty = self.context.i32_type();
            enum_ty.set_body(&[tag_ty.into(), payload_ty.into()], false);
        } else {
            // Wide layout: { i32 tag, [WORDS x i64] }. Payloads roundtrip
            // through word buffers (see construction / match lowering).
            // Own-containing payloads would bypass destruction tracking.
            let mut words = 0u64;
            for (vname, ftys) in &payload_tys {
                let mut total = 0u64;
                for fty in ftys {
                    if self.type_has_own_pair(fty, &mut HashSet::new()) {
                        return Err(CodegenError{message: format!("variant `{vname}` payload owns heap data; `own` in enum payloads needs structural destruction — rejected in phase 1"), span: e.name_span});
                    }
                    let Some(k) = Self::llvm_word_count(fty) else {
                        return Err(CodegenError{message: format!("variant `{vname}` payload type has no fixed size for enum lowering"), span: e.name_span});
                    };
                    total += k;
                }
                words = words.max(total);
            }
            let words = words.max(1) as u32;
            let payload_arr = self.context.i64_type().array_type(words);
            enum_ty.set_body(&[self.context.i32_type().into(), payload_arr.into()], false);
            self.enum_wide_words.insert(e.name.clone(), words);
        }
        self.enum_payload_tys.insert(e.name.clone(), payload_tys);
        self.enum_types.insert(e.name.clone(), enum_ty);
        let mut tag_map = std::collections::HashMap::new();
        for (idx, v) in e.variants.iter().enumerate() {
            let tag = if let Some(expr) = &v.discriminant {
                if let ExprKind::IntLit(val) = &expr.kind {
                    *val as u32
                } else {
                    // For non-literal discriminant like `A = 5 + 3`, we could evaluate, but for MVP use idx
                    idx as u32
                }
            } else {
                idx as u32
            };
            tag_map.insert(v.name.clone(), tag);
        }
        self.enum_variant_tags.insert(e.name.clone(), tag_map);
        Ok(())
    }

    fn declare_typedef(&mut self, td: &TypedefDecl) -> Result<(), CodegenError> {
        let _ = self.llvm_ty_for(&td.ty);
        Ok(())
    }

    fn declare_distinct(&mut self, dd: &DistinctDecl) -> Result<(), CodegenError> {
        let _ = self.llvm_ty_for(&dd.ty);
        if !self.struct_types.contains_key(&dd.name) {
            let st = self.context.opaque_struct_type(&dd.name);
            let inner = self.llvm_ty_for(&dd.ty);
            st.set_body(&[inner], false);
            let mut map = std::collections::HashMap::new();
            map.insert("value".to_string(), 0);
            self.struct_types.insert(dd.name.clone(), st);
            self.struct_fields.insert(dd.name.clone(), map);
        }
        Ok(())
    }

     fn declare_extension(&mut self, ext: &ExtensionDecl) -> Result<(), CodegenError> {
        let target = match &ext.ty {
            Type::Named(n, _) => n.clone(),
            Type::Generic(n, _, _) => n.clone(),
            _ => return Ok(()),
        };
        // Ensure target struct exists
        let _ = self.llvm_ty_for(&ext.ty);
        // Handle field extensions: add to struct type
        for mem in &ext.members {
            if let crate::ast::ExtensionMember::Field(field) = mem {
                if let Some(st) = self.struct_types.get(&target).cloned() {
                    // Need to update struct type to include new field
                    // Get current field types plus new
                    let mut field_tys: Vec<BasicTypeEnum<'ctx>> = Vec::new();
                    let count = st.count_fields();
                    for i in 0..count {
                        field_tys.push(st.get_field_type_at_index(i).unwrap());
                    }
                    let new_ty = self.llvm_ty_for(&field.ty);
                    field_tys.push(new_ty);
                    // Update field map
                    let field_idx = field_tys.len() as u32 - 1;
                    self.struct_fields.entry(target.clone()).or_insert_with(HashMap::new).insert(field.name.clone(), field_idx);
                    // Update defaults
                    if let Some(def) = &field.default {
                        self.struct_field_defaults.entry(target.clone()).or_insert_with(HashMap::new).insert(field.name.clone(), def.clone());
                    }
                    let _ = st.set_body(&field_tys, false);
                }
            }
        }
        // For each function/operator/property/conversion member, declare as method of target
        for mem in &ext.members {
            match mem {
                crate::ast::ExtensionMember::Function(f) => {
                    let ret_ty_raw: crate::sema::Ty = (&f.ret_ty).into();
                    let ret_ty = self.resolve_ty_for_codegen(&ret_ty_raw);
                    let mut param_semas: Vec<crate::sema::Ty> = vec![crate::sema::Ty::Struct(target.clone())];
                    for (idx, pp) in f.params.iter().enumerate() {
                        let raw: crate::sema::Ty = (&pp.ty).into();
                        let res = self.resolve_ty_for_codegen(&raw);
                        let final_ty = if pp.is_variadic {
                            if pp.ty.name() == "__derived__" {
                                if idx == 0 { crate::sema::Ty::Array(Box::new(crate::sema::Ty::Int)) } else {
                                    let prev_raw: crate::sema::Ty = (&f.params[idx-1].ty).into();
                                    crate::sema::Ty::Array(Box::new(self.resolve_ty_for_codegen(&prev_raw)))
                                }
                            } else { crate::sema::Ty::Array(Box::new(res)) }
                        } else { res };
                        param_semas.push(final_ty);
                    }
                    let this_ty = self.context.ptr_type(inkwell::AddressSpace::default()).into();
                    let mut param_llvm: Vec<inkwell::types::BasicMetadataTypeEnum> = vec![this_ty];
                    for (idx, pp) in f.params.iter().enumerate() {
                        let t: crate::sema::Ty = (&pp.ty).into();
                        let sema_t = if pp.is_variadic {
                            if pp.ty.name() == "__derived__" {
                                if idx == 0 { crate::sema::Ty::Array(Box::new(crate::sema::Ty::Int)) } else {
                                    let prev_raw: crate::sema::Ty = (&f.params[idx-1].ty).into();
                                    crate::sema::Ty::Array(Box::new(self.resolve_ty_for_codegen(&prev_raw)))
                                }
                            } else { crate::sema::Ty::Array(Box::new(self.resolve_ty_for_codegen(&t))) }
                        } else { self.resolve_ty_for_codegen(&t) };
                        // `ref`/`out` params take opaque pointers (mirrors `declare_function`).
                        if pp.mode != ParamMode::None {
                            param_llvm.push(self.context.ptr_type(inkwell::AddressSpace::default()).into());
                        } else if let Some(bt) = self.llvm_ty_for_sema(&sema_t) { param_llvm.push(bt.into()); }
                    }
                    let fn_ty = match ret_ty {
                        crate::sema::Ty::Void => self.context.void_type().fn_type(&param_llvm, false),
                        crate::sema::Ty::Int => self.context.i64_type().fn_type(&param_llvm, false),
                crate::sema::Ty::UInt => self.context.i64_type().fn_type(&param_llvm, false),
                crate::sema::Ty::SizedInt { bits, .. } => self.llvm_int_for_bits(bits).fn_type(&param_llvm, false),
                        crate::sema::Ty::Bool => self.context.bool_type().fn_type(&param_llvm, false),
                        crate::sema::Ty::Char => self.context.i32_type().fn_type(&param_llvm, false),
                        crate::sema::Ty::String => self.context.ptr_type(inkwell::AddressSpace::default()).fn_type(&param_llvm, false),
                        crate::sema::Ty::Float => self.context.f32_type().fn_type(&param_llvm, false),
                        crate::sema::Ty::Double => self.context.f64_type().fn_type(&param_llvm, false),
                        crate::sema::Ty::Struct(ref n) => {
                            if let Some(pair) = self.trait_pair_of(n) {
                                pair.fn_type(&param_llvm, false)
                            } else {
                                let st = self.struct_types.get(n).unwrap();
                                st.fn_type(&param_llvm, false)
                            }
                        }
                        crate::sema::Ty::Own(ref inner) => {
                            self.own_pair_type(inner).fn_type(&param_llvm, false)
                        }
                        // `task<T>` (Async-6): opaque runtime handle pointer.
                        crate::sema::Ty::Task(_) => self.context.ptr_type(inkwell::AddressSpace::default()).fn_type(&param_llvm, false),
                        crate::sema::Ty::Enum(ref n) => {
                            let et = self.enum_types.get(n).unwrap();
                            et.fn_type(&param_llvm, false)
                        }
                        crate::sema::Ty::Generic(_, _) => self.context.ptr_type(inkwell::AddressSpace::default()).fn_type(&param_llvm, false),
                        crate::sema::Ty::Tuple(ref tys) => self.tuple_struct_ty(tys).map(|st| st.fn_type(&param_llvm, false)).unwrap_or_else(|| self.context.ptr_type(inkwell::AddressSpace::default()).fn_type(&param_llvm, false)),
                        crate::sema::Ty::Any => self.context.ptr_type(inkwell::AddressSpace::default()).fn_type(&param_llvm, false),
                        crate::sema::Ty::Function(_, _) => self.context.ptr_type(inkwell::AddressSpace::default()).fn_type(&param_llvm, false),
                        crate::sema::Ty::Array(_) => self.context.i64_type().array_type(16).fn_type(&param_llvm, false),
                crate::sema::Ty::FixedArray { elem: ref elem, size: ref size } => {
                    let n = size.unwrap_or(16) as u32;
                    match self.llvm_ty_for_sema(elem.as_ref()) {
                        Some(BasicTypeEnum::IntType(it)) => it.array_type(n).fn_type(&param_llvm, false),
                        Some(BasicTypeEnum::FloatType(ft)) => ft.array_type(n).fn_type(&param_llvm, false),
                        Some(BasicTypeEnum::PointerType(pt)) => pt.array_type(n).fn_type(&param_llvm, false),
                        Some(BasicTypeEnum::StructType(st)) => st.array_type(n).fn_type(&param_llvm, false),
                        Some(BasicTypeEnum::ArrayType(at)) => at.array_type(n).fn_type(&param_llvm, false),
                        _ => self.context.i64_type().array_type(n).fn_type(&param_llvm, false),
                    }
                },
                crate::sema::Ty::Vec(ref elem) => {
                    let inner = match elem.as_ref() {
                        crate::sema::Ty::Any => self.context.i64_type().into(),
                        _ => self.llvm_ty_for_sema(elem).unwrap_or_else(|| self.context.i64_type().into()),
                    };
                    self.vec_struct_ty(inner).fn_type(&param_llvm, false)
                },
                crate::sema::Ty::Map { key: ref key, value: ref value } => {
                    let k = match key.as_ref() {
                        crate::sema::Ty::Any => self.context.i64_type().into(),
                        _ => self.llvm_ty_for_sema(key.as_ref()).unwrap_or_else(|| self.context.i64_type().into()),
                    };
                    let v = match value.as_ref() {
                        crate::sema::Ty::Any => self.context.ptr_type(inkwell::AddressSpace::default()).into(),
                        _ => self.llvm_ty_for_sema(value.as_ref()).unwrap_or_else(|| self.context.i64_type().into()),
                    };
                    self.map_struct_ty(k, v).fn_type(&param_llvm, false)
                },
                        crate::sema::Ty::Pointer(_) => self.context.ptr_type(inkwell::AddressSpace::default()).fn_type(&param_llvm, false),
                        crate::sema::Ty::Optional(ref el) => {
                            let inner = self.llvm_ty_for_sema(el).unwrap();
                            self.context.struct_type(&[inner.into(), self.context.bool_type().into()], false).fn_type(&param_llvm, false)
                        }
                    };
                    let mangled = format!("{}__{}", target, f.name);
                    let func = self.module.add_function(&mangled, fn_ty, None);
                    let entry = self.class_methods.entry(target.clone()).or_insert_with(std::collections::HashMap::new);
                    let mut full_names = vec!["this".to_string()];
                    full_names.extend(f.params.iter().map(|p| p.name.clone()));
                    let mut full_modes = vec![ParamMode::None];
                    full_modes.extend(f.params.iter().map(|p| p.mode));
                    let mut full_variadic = vec![false];
                    full_variadic.extend(f.params.iter().map(|p| p.is_variadic));
            let mut full_defaults = vec![None];
            full_defaults.extend(f.params.iter().map(|p| p.default.clone()));
                    entry.insert(f.name.clone(), (func, TyInfo{ret: ret_ty, params: param_semas.clone(), param_modes: full_modes, param_names: full_names, param_is_variadic: full_variadic, param_defaults: full_defaults, is_async: false}));
                }
                crate::ast::ExtensionMember::Operator(op) => {
                    let ret_ty = crate::sema::Ty::Int;
                    let mut param_semas = vec![crate::sema::Ty::Struct(target.clone())];
                    for (idx, pp) in op.params.iter().enumerate() {
                        let raw: crate::sema::Ty = (&pp.ty).into();
                        let res = self.resolve_ty_for_codegen(&raw);
                        let final_ty = if pp.is_variadic {
                            if pp.ty.name() == "__derived__" {
                                if idx == 0 { crate::sema::Ty::Array(Box::new(crate::sema::Ty::Int)) } else {
                                    let prev_raw: crate::sema::Ty = (&op.params[idx-1].ty).into();
                                    crate::sema::Ty::Array(Box::new(self.resolve_ty_for_codegen(&prev_raw)))
                                }
                            } else { crate::sema::Ty::Array(Box::new(res)) }
                        } else { res };
                        param_semas.push(final_ty);
                    }
                    let this_ty = self.context.ptr_type(inkwell::AddressSpace::default()).into();
                    let mut param_llvm: Vec<inkwell::types::BasicMetadataTypeEnum> = vec![this_ty];
                    for (idx, pp) in op.params.iter().enumerate() {
                        let t: crate::sema::Ty = (&pp.ty).into();
                        let sema_t = if pp.is_variadic {
                            if pp.ty.name() == "__derived__" {
                                if idx == 0 { crate::sema::Ty::Array(Box::new(crate::sema::Ty::Int)) } else {
                                    let prev_raw: crate::sema::Ty = (&op.params[idx-1].ty).into();
                                    crate::sema::Ty::Array(Box::new(self.resolve_ty_for_codegen(&prev_raw)))
                                }
                            } else { crate::sema::Ty::Array(Box::new(self.resolve_ty_for_codegen(&t))) }
                        } else { self.resolve_ty_for_codegen(&t) };
                        // `ref`/`out` params take opaque pointers (mirrors `declare_function`).
                        if pp.mode != ParamMode::None {
                            param_llvm.push(self.context.ptr_type(inkwell::AddressSpace::default()).into());
                        } else if let Some(bt) = self.llvm_ty_for_sema(&sema_t) { param_llvm.push(bt.into()); }
                    }
                    let fn_ty = match ret_ty {
                        crate::sema::Ty::Int => self.context.i64_type().fn_type(&param_llvm, false),
                crate::sema::Ty::UInt => self.context.i64_type().fn_type(&param_llvm, false),
                crate::sema::Ty::SizedInt { bits, .. } => self.llvm_int_for_bits(bits).fn_type(&param_llvm, false),
                        crate::sema::Ty::Bool => self.context.bool_type().fn_type(&param_llvm, false),
                        _ => self.context.i64_type().fn_type(&param_llvm, false),
                    };
                    let op_mangled = match op.op.as_str() {
                        "+" => "plus", "-" => "minus", "*" => "star", "/" => "slash", "%" => "percent",
                        "<" => "lt", "<=" => "le", ">" => "gt", ">=" => "ge",
                        "is" => "is", "is not" => "is_not",
                        "&" => "bitand", "|" => "bitor", "^" => "xor", "~" => "tilde",
                        "<<" => "lshift", ">>" => "rshift", "=" => "assign", "[]" => "index",
                        "++" => "inc", "--" => "dec",
                        "+=" => "plus_assign", "-=" => "minus_assign", "*=" => "star_assign", "/=" => "slash_assign", "%=" => "percent_assign",
                        "&=" => "and_assign", "|=" => "or_assign", "^=" => "xor_assign", "<<=" => "lshift_assign", ">>=" => "rshift_assign",
                        _ => "op",
                    };
                    let mangled = format!("{}__op_{}", target, op_mangled);
                    let func = self.module.add_function(&mangled, fn_ty, None);
                    let mut full_names = vec!["this".to_string()];
                    full_names.extend(op.params.iter().map(|p| p.name.clone()));
                    let mut full_modes = vec![ParamMode::None];
                    full_modes.extend(op.params.iter().map(|p| p.mode));
                    let mut full_variadic = vec![false];
                    full_variadic.extend(op.params.iter().map(|p| p.is_variadic));
            let mut full_defaults = vec![None];
            full_defaults.extend(op.params.iter().map(|p| p.default.clone()));
                    let entry = self.class_operators.entry(target.clone()).or_insert_with(HashMap::new);
                    entry.insert(op.op.clone(), (func, TyInfo{ret: ret_ty.clone(), params: param_semas.clone(), param_modes: full_modes, param_names: full_names, param_is_variadic: full_variadic, param_defaults: full_defaults, is_async: false}));
                }
                crate::ast::ExtensionMember::Property(prop) => {
                    let prop_ty_raw: crate::sema::Ty = prop.ty.as_ref().map(|t| t.into()).or_else(|| prop.setter.as_ref().map(|(p,_)| (&p.ty).into())).unwrap_or(crate::sema::Ty::Int);
                    let prop_ty = self.resolve_ty_for_codegen(&prop_ty_raw);
                    let mut pg = None;
                    let mut ps = None;
                    if prop.getter.is_some() {
                        let ret_llvm = self.llvm_ty_for_sema(&prop_ty).unwrap();
                        let this_ty = self.context.ptr_type(inkwell::AddressSpace::default()).into();
                        let fn_ty = match prop_ty {
                            crate::sema::Ty::Void => self.context.void_type().fn_type(&[this_ty], false),
                            _ => ret_llvm.fn_type(&[this_ty], false),
                        };
                        let mangled = format!("{}__get_{}", target, prop.name);
                        let func = if let Some(existing) = self.class_properties.get(&target).and_then(|m| m.get(&prop.name)).and_then(|pc| pc.getter.as_ref().map(|(f,_)| *f)) {
                            existing
                        } else {
                            self.module.add_function(&mangled, fn_ty, None)
                        };
                        let mut params = vec![crate::sema::Ty::Struct(target.clone())];
                        pg = Some((func, TyInfo{ret: prop_ty.clone(), params: params.clone(), param_modes: vec![ParamMode::None; params.len()], param_names: Vec::new(), param_is_variadic: Vec::new(), param_defaults: Vec::new(), is_async: false}));
                    }
                    if let Some((ref param,_)) = prop.setter {
                        let setter_ty_raw: crate::sema::Ty = (&param.ty).into();
                        let setter_ty = self.resolve_ty_for_codegen(&setter_ty_raw);
                        let this_ty = self.context.ptr_type(inkwell::AddressSpace::default()).into();
                        let val_llvm = self.llvm_ty_for_sema(&setter_ty).unwrap();
                        let fn_ty = self.context.void_type().fn_type(&[this_ty, val_llvm.into()], false);
                        let mangled = format!("{}__set_{}", target, prop.name);
                        let func = if let Some(existing) = self.class_properties.get(&target).and_then(|m| m.get(&prop.name)).and_then(|pc| pc.setter.as_ref().map(|(f,_)| *f)) {
                            existing
                        } else {
                            self.module.add_function(&mangled, fn_ty, None)
                        };
                        let mut params = vec![crate::sema::Ty::Struct(target.clone()), setter_ty.clone()];
                        ps = Some((func, TyInfo{ret: crate::sema::Ty::Void, params: params.clone(), param_modes: vec![ParamMode::None; params.len()], param_names: Vec::new(), param_is_variadic: Vec::new(), param_defaults: Vec::new(), is_async: false}));
                    }
                    let entry = self.class_properties.entry(target.clone()).or_insert_with(HashMap::new);
                    if let Some(existing) = entry.get(&prop.name).cloned() {
                        let mut merged_getter = existing.getter.clone();
                        let mut merged_setter = existing.setter.clone();
                        if pg.is_some() && merged_getter.is_none() { merged_getter = pg.clone(); }
                        if ps.is_some() && merged_setter.is_none() { merged_setter = ps.clone(); }
                        let merged_ty = if existing.ty != crate::sema::Ty::Int { existing.ty.clone() } else { prop_ty.clone() };
                        entry.insert(prop.name.clone(), PropertyCG{ty: merged_ty, getter: merged_getter, setter: merged_setter});
                    } else {
                        entry.insert(prop.name.clone(), PropertyCG{ty: prop_ty, getter: pg, setter: ps});
                    }
                }
                crate::ast::ExtensionMember::Conversion(conv) => {
                    let from_ty: crate::sema::Ty = (&conv.from_ty).into();
                    let to_ty: crate::sema::Ty = (&conv.to_ty).into();
                    // For MVP, just create a placeholder function for conversion
                    let _ = self.resolve_ty_for_codegen(&from_ty);
                    let _ = self.resolve_ty_for_codegen(&to_ty);
                    // No need to declare function now, will be handled in codegen_conversion via mangled name
                }
                crate::ast::ExtensionMember::Field(_) => {} // already handled above
            }
        }
        Ok(())
    }

    fn declare_extern(&mut self, ext: &ExternDecl) -> Result<(), CodegenError> {
        for mem in &ext.members {
            match mem {
                crate::ast::ExternMember::Function{ty, name, params, ..} => {
                    // Real stdlib: the same libc symbol may be declared both by
                    // `stdlib/std/*.hll` and by user code (e.g. `printf` in
                    // `advanced.hll`). LLVM requires one declaration, so reuse
                    // the existing one when the name is already declared.
                    if self.module.get_function(name).is_some() {
                        continue;
                    }
                    let ret_ty: crate::sema::Ty = ty.into();
                    let mut is_c_varargs = params.iter().any(|p| p.is_variadic && p.name.is_empty());
                    // Special: `extern "c" from "libc" do int printf(string arg) end` declares `printf` with one `string` param
                    // but real C `printf` is variadic `int printf(const char*, ...)`.
                    // If user declares `printf` with single `string` param and non-variadic, treat it as variadic C `printf`.
                    if name == "printf" && params.len() == 1 && !is_c_varargs {
                        if let Some(p) = params.first() {
                            if matches!(&p.ty, Type::String(_)) {
                                is_c_varargs = true;
                            }
                        }
                    }
                    // For C `printf`, ensure variadic `i32 (ptr, ...)` even though Hella `int` maps to `i64`
                    if name == "printf" && is_c_varargs {
                        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default()).into();
                        let fn_ty = self.context.i32_type().fn_type(&[ptr_ty], true);
                        self.module.add_function(name, fn_ty, None);
                        if matches!(ret_ty, crate::sema::Ty::Int) {
                            self.extern_int32_rets.insert(name.clone());
                        }
                        continue;
                    }
                    // A Hella `int` return lowers to a true C `int` (i32) so
                    // negative returns (strcmp, scanf EOF, ...) keep their
                    // sign; callers sign-extend back to Hella `int` (i64).
                    // Exempted: libc functions whose real return is
                    // size_t/long (`strlen`, `fread`, `fwrite`, `ftell`),
                    // which stay i64 (non-negative values read correctly).
                    let wide_int_ret = matches!(ret_ty, crate::sema::Ty::Int)
                        && matches!(name.as_str(), "strlen" | "fread" | "fwrite" | "ftell");
                    let ret_is_c_int = matches!(ret_ty, crate::sema::Ty::Int) && !wide_int_ret;
                    if ret_is_c_int {
                        self.extern_int32_rets.insert(name.clone());
                    }
                    let param_tys: Vec<crate::sema::Ty> = params.iter().filter(|p| !(p.is_variadic && p.name.is_empty())).map(|p| {
                        let base: crate::sema::Ty = (&p.ty).into();
                        if p.is_variadic {
                            crate::sema::Ty::Array(Box::new(base))
                        } else { base }
                    }).collect();
                    let param_llvm: Vec<inkwell::types::BasicMetadataTypeEnum> = param_tys.iter().filter_map(|t| self.llvm_ty_for_sema(t).map(|bt| bt.into())).collect();
                    let fn_ty = match ret_ty {
                        crate::sema::Ty::Void => self.context.void_type().fn_type(&param_llvm, is_c_varargs),
                        crate::sema::Ty::Int if wide_int_ret => self.context.i64_type().fn_type(&param_llvm, is_c_varargs),
                        crate::sema::Ty::Int => self.context.i32_type().fn_type(&param_llvm, is_c_varargs),
                        crate::sema::Ty::UInt => self.context.i64_type().fn_type(&param_llvm, is_c_varargs),
                        crate::sema::Ty::SizedInt { bits, signed } => {
                            // Fixed-width extern returns lower to their natural C width
                            // (A4: previously fell through to void). 32-bit signed
                            // still needs sext tracking; 32-bit unsigned needs zext.
                            if bits == 32 && signed {
                                self.extern_int32_rets.insert(name.clone());
                            } else if bits == 32 && !signed {
                                self.extern_uint32_rets.insert(name.clone());
                            }
                            self.llvm_int_for_bits(bits).fn_type(&param_llvm, is_c_varargs)
                        }
                        crate::sema::Ty::Bool => self.context.bool_type().fn_type(&param_llvm, is_c_varargs),
                        crate::sema::Ty::Char => self.context.i32_type().fn_type(&param_llvm, is_c_varargs),
                        crate::sema::Ty::String => self.context.ptr_type(inkwell::AddressSpace::default()).fn_type(&param_llvm, is_c_varargs),
                        crate::sema::Ty::Float => self.context.f32_type().fn_type(&param_llvm, is_c_varargs),
                        crate::sema::Ty::Double => self.context.f64_type().fn_type(&param_llvm, is_c_varargs),
                        crate::sema::Ty::Struct(_) | crate::sema::Ty::Enum(_) | crate::sema::Ty::Generic(_,_) => {
                            if let Some(bt) = self.llvm_ty_for_sema(&ret_ty) { bt.fn_type(&param_llvm, is_c_varargs) } else { self.context.void_type().fn_type(&param_llvm, is_c_varargs) }
                        }
                        _ => self.context.void_type().fn_type(&param_llvm, is_c_varargs),
                    };
                    self.module.add_function(name, fn_ty, None);
                }
                crate::ast::ExternMember::Struct{name, fields, ..} => {
                    if self.struct_types.contains_key(name) { continue; }
                    let opaque = self.context.opaque_struct_type(name);
                    self.struct_types.insert(name.clone(), opaque);
                    let mut field_map = HashMap::new();
                    let mut field_tys = Vec::new();
                    for (idx, f) in fields.iter().enumerate() {
                        let lty = self.llvm_ty_for(&f.ty);
                        field_map.insert(f.name.clone(), idx as u32);
                        field_tys.push(lty);
                    }
                    opaque.set_body(&field_tys, false);
                    self.struct_fields.insert(name.clone(), field_map);
                    self.struct_field_defaults.insert(name.clone(), HashMap::new());
                }
                crate::ast::ExternMember::Enum{name, variants, ..} => {
                    if self.enum_types.contains_key(name) || self.struct_types.contains_key(name) { continue; }
                    let enum_ty = self.context.opaque_struct_type(name);
                    let payload_ty = self.context.i64_type();
                    let tag_ty = self.context.i32_type();
                    enum_ty.set_body(&[tag_ty.into(), payload_ty.into()], false);
                    self.enum_types.insert(name.clone(), enum_ty);
                    let mut tag_map = HashMap::new();
                    for (idx, v) in variants.iter().enumerate() {
                        let tag = if let Some(expr) = &v.discriminant {
                            if let ExprKind::IntLit(val) = &expr.kind { *val as u32 } else { idx as u32 }
                        } else { idx as u32 };
                        tag_map.insert(v.name.clone(), tag);
                    }
                    self.enum_variant_tags.insert(name.clone(), tag_map);
                }
                crate::ast::ExternMember::Const{ty, name, ..} => {
                    let lty = self.llvm_ty_for(ty);
                    let global = self.module.add_global(lty, None, name);
                    global.set_constant(true);
                    global.set_linkage(inkwell::module::Linkage::External);
                    // Bare-minimum FFI: a true external declaration must NOT
                    // carry an initializer. Emitting `= zero` would define the
                    // symbol locally (e.g. null `stderr`) and shadow libc's,
                    // so no `set_initializer` call here by design.
                    let ptr = global.as_pointer_value();
                    self.globals.insert(name.clone(), (ptr, lty));
                }
            }
        }
        Ok(())
    }

    fn declare_const(&mut self, c: &ConstDecl) -> Result<(), CodegenError> {
        // Top-level map constants lower exactly like global map variables
        // (const-folded entries), then flagged constant.
        if matches!(c.ty, Some(Type::Map { .. }))
            || matches!(&c.ty, None) && matches!(c.init.kind, ExprKind::MapLit { .. })
        {
            let ty = c.ty.clone().unwrap_or(Type::Any(Span::new(0, 0)));
            let fake = VarDecl {
                visibility: c.visibility.clone(),
                ty,
                name: c.name.clone(),
                name_span: c.name_span,
                init: Some(c.init.clone()),
                span: c.span,
            };
            self.declare_global_var(&fake)?;
            if let Some(g) = self.module.get_global(&c.name) {
                g.set_constant(true);
            }
            return Ok(());
        }
        let ty = if let Some(t) = &c.ty {
            self.llvm_ty_for(t)
        } else {
            // infer from init: simple for int/bool/string
            match &c.init.kind {
                ExprKind::IntLit(_) => self.context.i64_type().into(),
                ExprKind::BoolLit(_) => self.context.bool_type().into(),
                ExprKind::StringLit(_) => self.context.ptr_type(inkwell::AddressSpace::default()).into(),
                ExprKind::CharLit(_) => self.context.i32_type().into(),
                ExprKind::FloatLit(_) => self.context.f64_type().into(),
                _ => self.context.i64_type().into(),
            }
        };
        let global = self.module.add_global(ty, None, &c.name);
        global.set_constant(true);
        // For simple literals, set initializer directly; for complex, initializer will be set at runtime via hella.init (deferred)
        // Int literals are emitted in the target width (sized ints truncate/extend).
        let init_val = match &c.init.kind {
            ExprKind::IntLit(v) => match ty {
                BasicTypeEnum::IntType(it) => it.const_int(*v as u64, true).into(),
                _ => self.context.i64_type().const_int(*v as u64, true).into(),
            },
            ExprKind::BoolLit(b) => self.context.bool_type().const_int(if *b {1} else {0}, false).into(),
            ExprKind::StringLit(s) => {
                let str_val = self.context.const_string(s.as_bytes(), true);
                let str_ty = str_val.get_type();
                let str_global = self.module.add_global(str_ty, None, &format!("str.init.{}.{}", c.name, self.globals.len()));
                str_global.set_initializer(&str_val);
                str_global.set_constant(true);
                str_global.set_linkage(inkwell::module::Linkage::Private);
                let zero = self.context.i32_type().const_zero();
                let ptr = unsafe { str_global.as_pointer_value().const_gep(str_ty, &[zero, zero]) };
                ptr.as_basic_value_enum()
            }
            ExprKind::CharLit(ch) => self.context.i32_type().const_int(*ch as u64, false).into(),
            _ => ty.const_zero(),
        };
        if global.get_initializer().is_none() {
            global.set_initializer(&init_val);
        }
        global.set_linkage(inkwell::module::Linkage::External);
        let ptr = global.as_pointer_value();
        self.globals.insert(c.name.clone(), (ptr, ty));
        if matches!(c.ty, Some(Type::String(_))) {
            self.string_vars.insert(c.name.clone());
        }
        if c.ty.as_ref().is_some_and(Self::ast_ty_is_unsigned) {
            self.unsigned_vars.insert(c.name.clone());
        }
        Ok(())
    }

    fn declare_global_var(&mut self, v: &VarDecl) -> Result<(), CodegenError> {
        // Global maps: `{ keys, vals, len }` const struct. Literal entries
        // const-fold (literals; anything else zero-fills); otherwise zero.
        if matches!(&v.ty, Type::Map { .. })
            || matches!(&v.ty, Type::Any(_))
                && v.init.as_ref().is_some_and(|i| matches!(i.kind, ExprKind::MapLit { .. }))
        {
            let entries: &[(Expr, Expr)] = match &v.init {
                Some(init) => match &init.kind {
                    ExprKind::MapLit { entries, .. } => entries,
                    _ => &[],
                },
                None => &[],
            };
            let (dk, dv) = self.map_keyval_llvm_ty(&v.ty, entries);
            let map_st = self.map_struct_ty(dk, dv);
            let global = self.module.add_global(map_st.as_basic_type_enum(), None, &v.name);
            global.set_constant(false);
            // Const-fold entries; pad buffers to capacity.
            let fold_const = |e: &Expr, slot: BasicTypeEnum<'ctx>| -> BasicValueEnum<'ctx> {
                match &e.kind {
                    ExprKind::IntLit(val) => match slot {
                        BasicTypeEnum::IntType(it) => it.const_int(*val as u64, true).into(),
                        _ => self.context.i64_type().const_int(*val as u64, true).into(),
                    },
                    ExprKind::BoolLit(b) => match slot {
                        BasicTypeEnum::IntType(it) => it.const_int(if *b { 1 } else { 0 }, false).into(),
                        _ => self.context.bool_type().const_int(if *b { 1 } else { 0 }, false).into(),
                    },
                    ExprKind::CharLit(ch) => self.context.i32_type().const_int(*ch as u64, false).into(),
                    _ => slot.const_zero(),
                }
            };
            // NOTE: string keys need runtime globals; const-fold strings via
            // private globals like scalar string inits.
            let mut key_consts: Vec<BasicValueEnum<'ctx>> = Vec::new();
            let mut val_consts: Vec<BasicValueEnum<'ctx>> = Vec::new();
            for (k, val) in entries.iter() {
                let kc = match &k.kind {
                    ExprKind::StringLit(s) => {
                        let str_val = self.context.const_string(s.as_bytes(), true);
                        let str_ty = str_val.get_type();
                        let str_global = self.module.add_global(str_ty, None, &format!("str.mapkey.{}.{}", v.name, self.globals.len()));
                        str_global.set_initializer(&str_val);
                        str_global.set_constant(true);
                        str_global.set_linkage(inkwell::module::Linkage::Private);
                        let zero = self.context.i32_type().const_zero();
                        let ptr = unsafe { str_global.as_pointer_value().const_gep(str_ty, &[zero, zero]) };
                        let pv: BasicValueEnum<'ctx> = ptr.as_basic_value_enum();
                        self.coerce_to_ty(pv, dk)
                    }
                    _ => fold_const(k, dk),
                };
                key_consts.push(kc);
                let vc = match &val.kind {
                    ExprKind::StringLit(s) => {
                        let str_val = self.context.const_string(s.as_bytes(), true);
                        let str_ty = str_val.get_type();
                        let str_global = self.module.add_global(str_ty, None, &format!("str.mapval.{}.{}", v.name, self.globals.len()));
                        str_global.set_initializer(&str_val);
                        str_global.set_constant(true);
                        str_global.set_linkage(inkwell::module::Linkage::Private);
                        let zero = self.context.i32_type().const_zero();
                        let ptr = unsafe { str_global.as_pointer_value().const_gep(str_ty, &[zero, zero]) };
                        let pv: BasicValueEnum<'ctx> = ptr.as_basic_value_enum();
                        self.coerce_to_ty(pv, dv)
                    }
                    _ => fold_const(val, dv),
                };
                val_consts.push(vc);
            }
            let keys_arr = match map_st.get_field_type_at_index(0).unwrap() {
                BasicTypeEnum::ArrayType(at) => at,
                _ => unreachable!(),
            };
            let vals_arr = match map_st.get_field_type_at_index(1).unwrap() {
                BasicTypeEnum::ArrayType(at) => at,
                _ => unreachable!(),
            };
            // Pad to capacity with slot zeros.
            while key_consts.len() < Self::MAP_CAP as usize {
                key_consts.push(dk.const_zero());
            }
            while val_consts.len() < Self::MAP_CAP as usize {
                val_consts.push(dv.const_zero());
            }
            let keys_val: BasicValueEnum<'ctx> = match dk {
                BasicTypeEnum::IntType(it) => {
                    let mut ivs: Vec<inkwell::values::IntValue<'ctx>> = key_consts.iter().map(|cv| match cv {
                        BasicValueEnum::IntValue(iv) => *iv,
                        _ => it.const_zero(),
                    }).collect();
                    while ivs.len() < Self::MAP_CAP as usize {
                        ivs.push(it.const_zero());
                    }
                    ivs.truncate(Self::MAP_CAP as usize);
                    it.const_array(&ivs).into()
                }
                BasicTypeEnum::PointerType(pt) => {
                    let null = pt.const_null();
                    let mut pvs: Vec<PointerValue<'ctx>> = key_consts.iter().map(|cv| match cv {
                        BasicValueEnum::PointerValue(pv) => *pv,
                        _ => null,
                    }).collect();
                    while pvs.len() < Self::MAP_CAP as usize {
                        pvs.push(null);
                    }
                    pvs.truncate(Self::MAP_CAP as usize);
                    pt.const_array(&pvs).into()
                }
                _ => keys_arr.const_zero().into(),
            };
            let vals_val: BasicValueEnum<'ctx> = match dv {
                BasicTypeEnum::IntType(it) => {
                    let mut ivs: Vec<inkwell::values::IntValue<'ctx>> = val_consts.iter().map(|cv| match cv {
                        BasicValueEnum::IntValue(iv) => *iv,
                        _ => it.const_zero(),
                    }).collect();
                    while ivs.len() < Self::MAP_CAP as usize {
                        ivs.push(it.const_zero());
                    }
                    ivs.truncate(Self::MAP_CAP as usize);
                    it.const_array(&ivs).into()
                }
                BasicTypeEnum::PointerType(pt) => {
                    let null = pt.const_null();
                    let mut pvs: Vec<PointerValue<'ctx>> = val_consts.iter().map(|cv| match cv {
                        BasicValueEnum::PointerValue(pv) => *pv,
                        _ => null,
                    }).collect();
                    while pvs.len() < Self::MAP_CAP as usize {
                        pvs.push(null);
                    }
                    pvs.truncate(Self::MAP_CAP as usize);
                    pt.const_array(&pvs).into()
                }
                _ => vals_arr.const_zero().into(),
            };
            let len = (entries.len().min(Self::MAP_CAP as usize)) as u64;
            let init_val: BasicValueEnum<'ctx> = map_st.const_named_struct(&[
                keys_val.into(),
                vals_val.into(),
                self.context.i64_type().const_int(len, false).into(),
            ]).into();
            global.set_initializer(&init_val);
            global.set_linkage(inkwell::module::Linkage::External);
            let ptr = global.as_pointer_value();
            self.globals.insert(v.name.clone(), (ptr, map_st.into()));
            self.map_vars.insert(v.name.clone());
            return Ok(());
        }
        if let Type::Vec { .. } = &v.ty {
            let dest_elem_ty = self.vec_elem_llvm_ty(&v.ty);
            let vec_st = self.vec_struct_ty(dest_elem_ty);
            let arr_ty: BasicTypeEnum<'ctx> = vec_st.get_field_type_at_index(0).unwrap();
            let global = self.module.add_global(vec_st.as_basic_type_enum(), None, &v.name);
            global.set_constant(false);
            let init_val: BasicValueEnum<'ctx> = match &v.init {
                Some(init) if matches!(init.kind, ExprKind::ArrayLit(_)) => {
                    let elems = match &init.kind {
                        ExprKind::ArrayLit(elems) => elems,
                        _ => unreachable!(),
                    };
                    let len = elems.len().min(Self::VEC_CAP as usize);
                    let const_vals: Vec<BasicValueEnum<'ctx>> = elems
                        .iter()
                        .take(len)
                        .map(|e| match &e.kind {
                            ExprKind::IntLit(val) => match dest_elem_ty {
                                BasicTypeEnum::IntType(it) => {
                                    it.const_int(*val as u64, true).into()
                                }
                                _ => self.context.i64_type().const_int(*val as u64, true).into(),
                            },
                            ExprKind::BoolLit(b) => match dest_elem_ty {
                                BasicTypeEnum::IntType(it) => {
                                    it.const_int(if *b { 1 } else { 0 }, false).into()
                                }
                                _ => self.context.bool_type().const_int(if *b { 1 } else { 0 }, false).into(),
                            },
                            ExprKind::StringLit(s) => {
                                let str_val = self.context.const_string(s.as_bytes(), true);
                                let str_ty = str_val.get_type();
                                let str_global = self.module.add_global(str_ty, None, &format!("str.vec.{}.{}", v.name, self.globals.len()));
                                str_global.set_initializer(&str_val);
                                str_global.set_constant(true);
                                str_global.set_linkage(inkwell::module::Linkage::Private);
                                let zero = self.context.i32_type().const_zero();
                                let ptr = unsafe { str_global.as_pointer_value().const_gep(str_ty, &[zero, zero]) };
                                let pv: BasicValueEnum<'ctx> = ptr.as_basic_value_enum();
                                self.coerce_to_ty(pv, dest_elem_ty)
                            }
                            _ => dest_elem_ty.const_zero(),
                        })
                        .collect();
                    // Pad buffer to capacity, then build `{ buffer, len }`.
                    let mut padded = const_vals;
                    while padded.len() < Self::VEC_CAP as usize {
                        padded.push(dest_elem_ty.const_zero());
                    }
                    padded.truncate(Self::VEC_CAP as usize);
                    let buf_val: BasicValueEnum<'ctx> = match arr_ty {
                        BasicTypeEnum::ArrayType(at) => match dest_elem_ty {
                            BasicTypeEnum::IntType(it) => {
                                let ivs: Vec<inkwell::values::IntValue<'ctx>> = padded
                                    .iter()
                                    .map(|cv| match cv {
                                        BasicValueEnum::IntValue(iv) => *iv,
                                        _ => it.const_zero(),
                                    })
                                    .collect();
                                it.const_array(&ivs).into()
                            }
                            BasicTypeEnum::PointerType(pt) => {
                                let null = pt.const_null();
                                let pvs: Vec<PointerValue<'ctx>> = padded
                                    .iter()
                                    .map(|cv| match cv {
                                        BasicValueEnum::PointerValue(pv) => *pv,
                                        _ => null,
                                    })
                                    .collect();
                                pt.const_array(&pvs).into()
                            }
                            _ => at.const_zero().into(),
                        },
                        _ => arr_ty.const_zero(),
                    };
                    vec_st
                        .const_named_struct(&[
                            buf_val.into(),
                            self.context.i64_type().const_int(len as u64, false).into(),
                        ])
                        .into()
                }
                _ => vec_st.const_zero().into(),
            };
            global.set_initializer(&init_val);
            global.set_linkage(inkwell::module::Linkage::External);
            let ptr = global.as_pointer_value();
            self.globals.insert(v.name.clone(), (ptr, vec_st.into()));
            self.vec_vars.insert(v.name.clone());
            return Ok(());
        }
        // `any xs = vec[]` globals: i64-slot vector, length 0.
        if let (Type::Any(_), Some(init)) = (&v.ty, &v.init) {
            if matches!(init.kind, ExprKind::VecEmpty(_)) {
                let elem: BasicTypeEnum<'ctx> = self.context.i64_type().into();
                let vec_st = self.vec_struct_ty(elem);
            let global = self.module.add_global(vec_st.as_basic_type_enum(), None, &v.name);
                global.set_constant(false);
                let zero: BasicValueEnum<'ctx> = vec_st.const_zero().into();
                global.set_initializer(&zero);
                global.set_linkage(inkwell::module::Linkage::External);
                let ptr = global.as_pointer_value();
                self.globals.insert(v.name.clone(), (ptr, vec_st.into()));
                self.vec_vars.insert(v.name.clone());
                return Ok(());
            }
        }
        if let (
            Type::FixedArray { elem, size, .. },
            Some(init),
        ) = (&v.ty, &v.init)
        {
            if let ExprKind::ArrayLit(elems) = &init.kind {
                let n = size.unwrap_or(elems.len() as u64) as u32;
                let inner = self.llvm_ty_for(elem);
                let arr_ty = match inner {
                    BasicTypeEnum::IntType(it) => it.array_type(n).into(),
                    BasicTypeEnum::PointerType(pt) => pt.array_type(n).into(),
                    BasicTypeEnum::FloatType(ft) => ft.array_type(n).into(),
                    BasicTypeEnum::StructType(st) => st.array_type(n).into(),
                    BasicTypeEnum::ArrayType(at) => at.array_type(n).into(),
                    _ => self.context.i64_type().array_type(n).into(),
                };
                let global = self.module.add_global(arr_ty, None, &v.name);
                global.set_constant(false);
                // Const-fold literal elements (MVP: integer-like literals
                // coerced to the element width; anything else zero-fills).
                let zero_int = self.context.i64_type().const_int(0, false);
                let int_elems: Vec<inkwell::values::IntValue<'ctx>> = elems
                    .iter()
                    .map(|e| match &e.kind {
                        ExprKind::IntLit(val) => match inner {
                            BasicTypeEnum::IntType(it) => {
                                it.const_int(*val as u64, true)
                            }
                            _ => zero_int,
                        },
                        ExprKind::BoolLit(b) => self
                            .context
                            .bool_type()
                            .const_int(if *b { 1 } else { 0 }, false),
                        ExprKind::CharLit(ch) => {
                            self.context.i32_type().const_int(*ch as u64, false)
                        }
                        _ => match inner {
                            BasicTypeEnum::IntType(it) => it.const_zero(),
                            _ => zero_int,
                        },
                    })
                    .collect();
                let init_val: BasicValueEnum<'ctx> = match arr_ty {
                    BasicTypeEnum::ArrayType(at) => match inner {
                        BasicTypeEnum::IntType(it) => {
                            // Pad with zeros if explicit N > literal length.
                            let mut vals = int_elems.clone();
                            while vals.len() < n as usize {
                                vals.push(it.const_zero());
                            }
                            // Truncate if literal longer (sema already errored).
                            vals.truncate(n as usize);
                            it.const_array(&vals).into()
                        }
                        _ => at.const_zero().into(),
                    },
                    _ => arr_ty.const_zero(),
                };
                global.set_initializer(&init_val);
                global.set_linkage(inkwell::module::Linkage::External);
                let ptr = global.as_pointer_value();
                self.globals.insert(v.name.clone(), (ptr, arr_ty));
                // Fixed arrays of destructible elements: program-end entry
                // with element type and static length (expanded at emission).
                if let Type::FixedArray { elem, .. } = &v.ty {
                    if self.dtor_name_for_ast_ty(elem.as_ref()).is_some() {
                        if let BasicTypeEnum::ArrayType(at) = arr_ty {
                            let elem_ty = at.get_element_type();
                            self.global_dtors.push(GlobalDtor::Array {
                                slot: ptr,
                                elem_ty,
                                len: at.len(),
                            });
                        }
                    }
                }
                return Ok(());
            }
        }
        let ty = self.llvm_ty_for(&v.ty);
        let global = self.module.add_global(ty, None, &v.name);
        global.set_constant(false);
        // For simple literals, set initializer directly; for complex, zero and init via hella.init
        let init_val: BasicValueEnum<'ctx> = if let Some(init) = &v.init {
            match &init.kind {
                ExprKind::IntLit(val) => match ty {
                    BasicTypeEnum::IntType(it) => it.const_int(*val as u64, true).into(),
                    _ => self.context.i64_type().const_int(*val as u64, true).into(),
                },
                ExprKind::BoolLit(b) => self.context.bool_type().const_int(if *b {1} else {0}, false).into(),
                ExprKind::StringLit(s) => {
                    // Create a private global string and use its pointer as initializer for `string` global
                    let str_val = self.context.const_string(s.as_bytes(), true);
                    let str_ty = str_val.get_type();
                    let str_global = self.module.add_global(str_ty, None, &format!("str.init.{}.{}", v.name, self.globals.len()));
                    str_global.set_initializer(&str_val);
                    str_global.set_constant(true);
                    str_global.set_linkage(inkwell::module::Linkage::Private);
                    let zero = self.context.i32_type().const_zero();
                    // GEP to first element: ptr @str, 0, 0
                    let ptr = unsafe { str_global.as_pointer_value().const_gep(str_ty, &[zero, zero]) };
                    ptr.as_basic_value_enum()
                }
                ExprKind::CharLit(ch) => self.context.i32_type().const_int(*ch as u64, false).into(),
                ExprKind::FloatLit(s) => {
                    if let Ok(f) = s.parse::<f64>() {
                        self.context.f64_type().const_float(f).into()
                    } else {
                        ty.const_zero()
                    }
                }
                _ => ty.const_zero(),
            }
        } else {
            ty.const_zero()
        };
        if global.get_initializer().is_none() {
            global.set_initializer(&init_val);
        }
        global.set_linkage(inkwell::module::Linkage::External);
        let ptr = global.as_pointer_value();
        self.globals.insert(v.name.clone(), (ptr, ty));
        if matches!(&v.ty, Type::String(_)) {
            self.string_vars.insert(v.name.clone());
        }
        self.track_unsigned_var(&v.name, &v.ty);
        // Ownership tracking for program-end destruction.
        match &v.ty {
            Type::Own(inner, _) => {
                let inner_name = match inner.as_ref() {
                    Type::Named(n, _) => n.rsplit("::").next().unwrap_or(n).to_string(),
                    Type::Generic(n, _, _) => n.rsplit("::").next().unwrap_or(n).to_string(),
                    _ => String::new(),
                };
                if !inner_name.is_empty() {
                    self.global_owns.push((ptr, inner_name));
                }
            }
            _ => {
                if let Some(name) = self.dtor_name_for_ast_ty(&v.ty) {
                    self.global_dtors.push(GlobalDtor::One(ptr, name));
                }
            }
        }
        // Non-trivial initializers cannot const-fold: evaluate them at
        // program start (pending queue drained in `main`).
        if let Some(init) = &v.init {
            let trivial = matches!(
                init.kind,
                ExprKind::IntLit(_)
                    | ExprKind::BoolLit(_)
                    | ExprKind::CharLit(_)
                    | ExprKind::FloatLit(_)
                    | ExprKind::StringLit(_)
                    | ExprKind::Null
            );
            if !trivial {
                self.pending_global_inits.push((v.name.clone(), init.clone()));
            }
        }
        Ok(())
    }

    fn codegen_extension(&mut self, ext: &ExtensionDecl) -> Result<(), CodegenError> {
        let target = match &ext.ty {
            Type::Named(n, _) => n.clone(),
            Type::Generic(n, _, _) => n.clone(),
            _ => return Ok(()),
        };
        for mem in &ext.members {
            match mem {
                crate::ast::ExtensionMember::Function(f) => {
                    let mangled = format!("{}__{}", target, f.name);
                    let func = self.module.get_function(&mangled).ok_or(CodegenError{message: format!("extension func not declared {}", mangled), span: f.span})?;
                    self.cur_fn = Some(func);
                    self.cur_class = Some(target.clone());
                    let entry = self.context.append_basic_block(func, "entry");
                    self.builder.position_at_end(entry);
                    self.vars.push(std::collections::HashMap::new());
                    let this_ty: BasicTypeEnum<'ctx> = self.context.ptr_type(inkwell::AddressSpace::default()).into();
                    let this_param = func.get_nth_param(0).unwrap();
                    let this_alloca = self.create_entry_block_alloca("this", this_ty);
                    self.builder.build_store(this_alloca, this_param).unwrap();
                    self.vars.last_mut().unwrap().insert("this".to_string(), (this_alloca, this_ty));
                    for (i, param) in f.params.iter().enumerate() {
                        let llvm_ty = self.llvm_ty_for(&param.ty);
                        let val = func.get_nth_param((i+1) as u32).unwrap();
                        if param.mode != ParamMode::None {
                            // `ref`/`out`: the caller passed a pointer; use it directly.
                            let inner_ty = self.llvm_ty_for(&param.ty);
                            let ptr = val.into_pointer_value();
                            self.vars.last_mut().unwrap().insert(param.name.clone(), (ptr, inner_ty));
                        } else {
                            let alloca = self.create_entry_block_alloca(&param.name, llvm_ty);
                            self.builder.build_store(alloca, val).unwrap();
                            self.vars.last_mut().unwrap().insert(param.name.clone(), (alloca, llvm_ty));
                        }
                        if matches!(&param.ty, Type::Vec { .. }) { self.vec_vars.insert(param.name.clone()); }
                        if matches!(&param.ty, Type::Map { .. }) { self.map_vars.insert(param.name.clone()); }
                        if matches!(&param.ty, Type::String(_)) { self.string_vars.insert(param.name.clone()); }
                        self.track_unsigned_var(&param.name, &param.ty);
                    }
                    let _ = self.codegen_block(&f.body)?;
                    if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                        // Same implicit-return rule as class methods: `void`
                        // gets a bare `ret`, anything else a zero value.
                        // (Unconditional `ret i64 0` used to fail module
                        // verification for `void` extension methods.)
                        let ret_raw: crate::sema::Ty = (&f.ret_ty).into();
                        let ret_ty = self.resolve_ty_for_codegen(&ret_raw);
                        match self.default_return_value(&ret_ty) {
                            Some(zero) => { self.builder.build_return(Some(&zero)).unwrap(); }
                            None => { self.builder.build_return(None).unwrap(); }
                        }
                    }
                    self.vars.pop();
                    self.cur_fn = None;
                    self.cur_class = None;
                    if !func.verify(true) { return Err(CodegenError{message: format!("extension {}::{} verify failed", target, f.name), span: f.span}); }
                }
                crate::ast::ExtensionMember::Operator(op) => {
                    let op_mangled = match op.op.as_str() {
                        "+" => "plus", "-" => "minus", "*" => "star", "/" => "slash", "%" => "percent",
                        "<" => "lt", "<=" => "le", ">" => "gt", ">=" => "ge",
                        "is" => "is", "is not" => "is_not",
                        "&" => "bitand", "|" => "bitor", "^" => "xor", "~" => "tilde",
                        "<<" => "lshift", ">>" => "rshift", "=" => "assign", "[]" => "index",
                        "++" => "inc", "--" => "dec",
                        "+=" => "plus_assign", "-=" => "minus_assign", "*=" => "star_assign", "/=" => "slash_assign", "%=" => "percent_assign",
                        "&=" => "and_assign", "|=" => "or_assign", "^=" => "xor_assign", "<<=" => "lshift_assign", ">>=" => "rshift_assign",
                        _ => "op",
                    };
                    let mangled = format!("{}__op_{}", target, op_mangled);
                    let func = self.module.get_function(&mangled).ok_or(CodegenError{message: format!("extension op {} not declared {}", op.op, mangled), span: op.span})?;
                    self.cur_fn = Some(func);
                    self.cur_class = Some(target.clone());
                    let entry = self.context.append_basic_block(func, "entry");
                    self.builder.position_at_end(entry);
                    self.vars.push(HashMap::new());
                    let this_ty: BasicTypeEnum<'ctx> = self.context.ptr_type(inkwell::AddressSpace::default()).into();
                    let this_param = func.get_nth_param(0).unwrap();
                    let this_alloca = self.create_entry_block_alloca("this", this_ty);
                    self.builder.build_store(this_alloca, this_param).unwrap();
                    self.vars.last_mut().unwrap().insert("this".to_string(), (this_alloca, this_ty));
                    for (i, param) in op.params.iter().enumerate() {
                        let llvm_ty = self.llvm_ty_for(&param.ty);
                        let val = func.get_nth_param((i+1) as u32).unwrap();
                        if param.mode != ParamMode::None {
                            // `ref`/`out`: the caller passed a pointer; use it directly.
                            let inner_ty = self.llvm_ty_for(&param.ty);
                            let ptr = val.into_pointer_value();
                            self.vars.last_mut().unwrap().insert(param.name.clone(), (ptr, inner_ty));
                        } else {
                            let alloca = self.create_entry_block_alloca(&param.name, llvm_ty);
                            self.builder.build_store(alloca, val).unwrap();
                            self.vars.last_mut().unwrap().insert(param.name.clone(), (alloca, llvm_ty));
                        }
                        if matches!(&param.ty, Type::Vec { .. }) { self.vec_vars.insert(param.name.clone()); }
                        if matches!(&param.ty, Type::Map { .. }) { self.map_vars.insert(param.name.clone()); }
                        if matches!(&param.ty, Type::String(_)) { self.string_vars.insert(param.name.clone()); }
                        self.track_unsigned_var(&param.name, &param.ty);
                    }
                    let _ = self.codegen_block(&op.body)?;
                    if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                        self.builder.build_return(Some(&self.context.i64_type().const_int(0,false))).unwrap();
                    }
                    self.vars.pop();
                    self.cur_fn = None;
                    self.cur_class = None;
                    if !func.verify(true) { return Err(CodegenError{message: format!("extension op {} failed verify", op.op), span: op.span}); }
                }
                crate::ast::ExtensionMember::Property(prop) => {
                    if let Some(getter) = &prop.getter {
                        let mangled = format!("{}__get_{}", target, prop.name);
                        let func = self.module.get_function(&mangled).ok_or(CodegenError{message: format!("extension getter not declared {}", mangled), span: prop.span})?;
                        self.cur_fn = Some(func);
                        self.cur_class = Some(target.clone());
                        let entry = self.context.append_basic_block(func, "entry");
                        self.builder.position_at_end(entry);
                        self.vars.push(HashMap::new());
                        let this_ty: BasicTypeEnum<'ctx> = self.context.ptr_type(inkwell::AddressSpace::default()).into();
                        let this_param = func.get_nth_param(0).unwrap();
                        let this_alloca = self.create_entry_block_alloca("this", this_ty);
                        self.builder.build_store(this_alloca, this_param).unwrap();
                        self.vars.last_mut().unwrap().insert("this".to_string(), (this_alloca, this_ty));
                        let _ = self.codegen_block(getter)?;
                        if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                            if let Some(pty) = &prop.ty {
                                let _ty = self.llvm_ty_for(pty);
                                let zero: BasicValueEnum<'ctx> = match pty { Type::Int(_) => self.context.i64_type().const_int(0,false).into(), Type::Bool(_) => self.context.bool_type().const_int(0,false).into(), _ => self.context.i64_type().const_int(0,false).into() };
                                self.builder.build_return(Some(&zero)).unwrap();
                            } else { self.builder.build_return(None).unwrap(); }
                        }
                        self.vars.pop();
                        self.cur_fn = None;
                        self.cur_class = None;
                        if !func.verify(true) { return Err(CodegenError{message: format!("extension getter {} failed verify", mangled), span: prop.span}); }
                    }
                    if let Some((param, body)) = &prop.setter {
                        let mangled = format!("{}__set_{}", target, prop.name);
                        let func = self.module.get_function(&mangled).ok_or(CodegenError{message: format!("extension setter not declared {}", mangled), span: prop.span})?;
                        self.cur_fn = Some(func);
                        self.cur_class = Some(target.clone());
                        let entry = self.context.append_basic_block(func, "entry");
                        self.builder.position_at_end(entry);
                        self.vars.push(HashMap::new());
                        let this_ty: BasicTypeEnum<'ctx> = self.context.ptr_type(inkwell::AddressSpace::default()).into();
                        let this_param = func.get_nth_param(0).unwrap();
                        let this_alloca = self.create_entry_block_alloca("this", this_ty);
                        self.builder.build_store(this_alloca, this_param).unwrap();
                        self.vars.last_mut().unwrap().insert("this".to_string(), (this_alloca, this_ty));
                        let llvm_ty = self.llvm_ty_for(&param.ty);
                        let alloca = self.create_entry_block_alloca(&param.name, llvm_ty);
                        let val = func.get_nth_param(1).unwrap();
                        self.builder.build_store(alloca, val).unwrap();
                        self.vars.last_mut().unwrap().insert(param.name.clone(), (alloca, llvm_ty));
                        if matches!(&param.ty, Type::Vec { .. }) { self.vec_vars.insert(param.name.clone()); }
                        if matches!(&param.ty, Type::Map { .. }) { self.map_vars.insert(param.name.clone()); }
                        if matches!(&param.ty, Type::String(_)) { self.string_vars.insert(param.name.clone()); }
                        self.track_unsigned_var(&param.name, &param.ty);
                        let _ = self.codegen_block(body)?;
                        if self.builder.get_insert_block().unwrap().get_terminator().is_none() { self.builder.build_return(None).unwrap(); }
                        self.vars.pop();
                        self.cur_fn = None;
                        self.cur_class = None;
                        if !func.verify(true) { return Err(CodegenError{message: format!("extension setter {} failed verify", mangled), span: prop.span}); }
                    }
                }
                crate::ast::ExtensionMember::Conversion(conv) => {
                    let mangled = format!("{}__conv_{}_to_{}", target, conv.from_ty.name().replace("<","_").replace(">","_").replace(",","_"), conv.to_ty.name().replace("<","_").replace(">","_").replace(",","_"));
                    // Type by declared target (see `codegen_conversion`).
                    let to_sema: crate::sema::Ty = (&conv.to_ty).into();
                    let to_resolved = self.resolve_ty_for_codegen(&to_sema);
                    let ret_llvm: BasicTypeEnum<'ctx> = self.llvm_ty_for_sema(&to_resolved).unwrap_or_else(|| self.context.i64_type().into());
                    let func = self.module.get_function(&mangled).unwrap_or_else(|| {
                        let fn_ty = ret_llvm.fn_type(&[self.context.ptr_type(inkwell::AddressSpace::default()).into()], false);
                        self.module.add_function(&mangled, fn_ty, None)
                    });
                    self.cur_fn = Some(func);
                    self.cur_class = Some(target.clone());
                    let entry = self.context.append_basic_block(func, "entry");
                    self.builder.position_at_end(entry);
                    self.vars.push(HashMap::new());
                    self.own_slots.push(Vec::new());
                    let this_ty: BasicTypeEnum<'ctx> = self.context.ptr_type(inkwell::AddressSpace::default()).into();
                    let this_param = func.get_nth_param(0).unwrap();
                    let this_alloca = self.create_entry_block_alloca("this", this_ty);
                    self.builder.build_store(this_alloca, this_param).unwrap();
                    self.vars.last_mut().unwrap().insert("this".to_string(), (this_alloca, this_ty));
                    let _ = self.codegen_block(&conv.body)?;
                    if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                        self.emit_current_scope_owns();
                        match self.default_return_value(&to_resolved) {
                            Some(zero) => {
                                let cz = self.coerce_to_ty(zero, ret_llvm);
                                self.builder.build_return(Some(&cz)).unwrap();
                            }
                            None => {
                                let z: BasicValueEnum<'ctx> = self.context.i64_type().const_zero().into();
                                let cz = self.coerce_to_ty(z, ret_llvm);
                                self.builder.build_return(Some(&cz)).unwrap();
                            }
                        }
                    }
                    self.own_slots.pop();
                    self.vars.pop();
                    self.cur_fn = None;
                    self.cur_class = None;
                }
                crate::ast::ExtensionMember::Field(_) => {} // already handled in declare
            }
        }
        Ok(())
    }

    fn codegen_init(&mut self, blk: &Block) -> Result<(), CodegenError> {
        let init_fn = self.module.get_function("hella.init").unwrap_or_else(|| {
            let fn_ty = self.context.void_type().fn_type(&[], false);
            self.module.add_function("hella.init", fn_ty, None)
        });
        let entry = self.context.append_basic_block(init_fn, "entry");
        self.builder.position_at_end(entry);
        self.vars.push(std::collections::HashMap::new());
        self.cur_fn = Some(init_fn);
        let _ = self.codegen_block(blk)?;
        if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
            self.builder.build_return(None).unwrap();
        }
        self.vars.pop();
        self.cur_fn = None;
        Ok(())
    }

    fn llvm_int_for_bits(&self, bits: u16) -> inkwell::types::IntType<'ctx> {
        match bits {
            8 => self.context.i8_type(),
            16 => self.context.i16_type(),
            32 => self.context.i32_type(),
            64 => self.context.i64_type(),
            128 => self.context.i128_type(),
            _ => self.context.i64_type(),
        }
    }

    /// Coerce an integer value to a destination LLVM type via trunc/sext/zext.
    /// Non-integer or same-type values pass through unchanged. Used so that
    /// `i32 x = 5` (i64 literal → i32 slot) emits valid IR with opaque ptrs
    /// (the verifier cannot catch width mismatches on `ptr` stores).
    /// Also converts int↔pointer (via inttoptr/ptrtoint) for undetermined
    /// (`any`) vector slots, which are i64 and may hold string pointers.
    /// `unsigned_src` selects zero-extend (instead of sign-extend) when
    /// widening — required for `u8..u128`/`uint` values (A4 ABI fix; LLVM
    /// ints are signless so the Hella signedness must pick sext vs zext).
    fn coerce_to_ty_with_unsigned(
        &self,
        val: BasicValueEnum<'ctx>,
        dest: BasicTypeEnum<'ctx>,
        unsigned_src: bool,
    ) -> BasicValueEnum<'ctx> {
        let src = val.get_type();
        if src == dest {
            return val;
        }
        match (src, dest) {
            (BasicTypeEnum::IntType(s), BasicTypeEnum::IntType(d)) => {
                let sw = s.get_bit_width();
                let dw = d.get_bit_width();
                let iv = val.into_int_value();
                if sw > dw {
                    self.builder.build_int_truncate(iv, d, "trunc").unwrap().into()
                } else if sw < dw {
                    if unsigned_src {
                        self.builder.build_int_z_extend(iv, d, "zext").unwrap().into()
                    } else {
                        self.builder.build_int_s_extend(iv, d, "sext").unwrap().into()
                    }
                } else {
                    val
                }
            }
            (BasicTypeEnum::IntType(_), BasicTypeEnum::PointerType(d)) => self
                .builder
                .build_int_to_ptr(val.into_int_value(), d, "inttoptr")
                .unwrap()
                .into(),
            (BasicTypeEnum::PointerType(_), BasicTypeEnum::IntType(d)) => self
                .builder
                .build_ptr_to_int(val.into_pointer_value(), d, "ptrtoint")
                .unwrap()
                .into(),
            // Lift into Optional `{value, true}`: dest is a two-field struct
            // whose second field is `i1` and whose first field matches the
            // source type. (Sema only produces such flows for `T` → `T?`.)
            (_, BasicTypeEnum::StructType(dst_st))
                if dst_st.count_fields() == 2
                    && matches!(dst_st.get_field_type_at_index(1), Some(BasicTypeEnum::IntType(flag)) if flag.get_bit_width() == 1)
                    && dst_st.get_field_type_at_index(0) == Some(src) =>
            {
                let mut agg: BasicValueEnum<'ctx> = dst_st.get_undef().into();
                let tmp = self
                    .builder
                    .build_insert_value(agg.into_struct_value(), val, 0, "opt.val")
                    .unwrap();
                agg = tmp.as_basic_value_enum();
                let present = self.context.bool_type().const_int(1, false);
                let tmp2 = self
                    .builder
                    .build_insert_value(agg.into_struct_value(), present, 1, "opt.some")
                    .unwrap();
                tmp2.as_basic_value_enum()
            }
            _ => val,
        }
    }

    fn coerce_to_ty(
        &self,
        val: BasicValueEnum<'ctx>,
        dest: BasicTypeEnum<'ctx>,
    ) -> BasicValueEnum<'ctx> {
        self.coerce_to_ty_with_unsigned(val, dest, false)
    }

    /// Unify two integer operands to the wider width (sext/zext the narrower).
    /// Non-integer pairs pass through unchanged. `either_unsigned` selects
    /// zero-extend when widening — callers pass true when either side is a
    /// `u8..u128`/`uint` value (A4 ABI fix).
    fn unify_int_operands_with_unsigned(
        &self,
        l: BasicValueEnum<'ctx>,
        r: BasicValueEnum<'ctx>,
        either_unsigned: bool,
    ) -> (BasicValueEnum<'ctx>, BasicValueEnum<'ctx>) {
        match (l.get_type(), r.get_type()) {
            (BasicTypeEnum::IntType(lt), BasicTypeEnum::IntType(rt)) => {
                let lw = lt.get_bit_width();
                let rw = rt.get_bit_width();
                if lw == rw {
                    (l, r)
                } else if lw < rw {
                    (self.coerce_to_ty_with_unsigned(l, r.get_type(), either_unsigned), r)
                } else {
                    (l, self.coerce_to_ty_with_unsigned(r, l.get_type(), either_unsigned))
                }
            }
            _ => (l, r),
        }
    }

    fn unify_int_operands(
        &self,
        l: BasicValueEnum<'ctx>,
        r: BasicValueEnum<'ctx>,
    ) -> (BasicValueEnum<'ctx>, BasicValueEnum<'ctx>) {
        self.unify_int_operands_with_unsigned(l, r, false)
    }

    /// Max elements in a vector buffer (fixed capacity for now; `push` past it
    /// traps via `abort`). Raised 16→256 as A2 working relief for compiler
    /// sources; heap-growable `{ptr,len,cap}` is the follow-up.
    const VEC_CAP: u32 = 256;

    /// Max entries in a map (fixed capacity for now; insert past it traps via
    /// `abort`, mirroring `push`). Raised 16→256 with VEC_CAP.
    const MAP_CAP: u32 = 256;

    /// Vector struct type `{ [CAP x E], i64 len }` for element LLVM type E.
    /// Anonymous structs are structurally uniqued by LLVM, so rebuilding per
    /// site is sound.
    fn vec_struct_ty(&self, elem: BasicTypeEnum<'ctx>) -> StructType<'ctx> {
        let buf: BasicTypeEnum<'ctx> = match elem {
            BasicTypeEnum::IntType(it) => it.array_type(Self::VEC_CAP).into(),
            BasicTypeEnum::FloatType(ft) => ft.array_type(Self::VEC_CAP).into(),
            BasicTypeEnum::PointerType(pt) => pt.array_type(Self::VEC_CAP).into(),
            BasicTypeEnum::StructType(st) => st.array_type(Self::VEC_CAP).into(),
            BasicTypeEnum::ArrayType(at) => at.array_type(Self::VEC_CAP).into(),
            _ => self.context.i64_type().array_type(Self::VEC_CAP).into(),
        };
        self.context.struct_type(
            &[buf.into(), self.context.i64_type().into()],
            false,
        )
    }

    /// LLVM struct type for a tuple type, or `None` when an element has no
    /// lowering. Tuples pass/return by value (multi-word struct), unlike
    /// `any`/function values which erase to pointers.
    fn tuple_struct_ty(
        &self,
        tys: &[crate::sema::Ty],
    ) -> Option<inkwell::types::StructType<'ctx>> {
        let mut elems = Vec::with_capacity(tys.len());
        for t in tys {
            elems.push(self.llvm_ty_for_sema(t)?);
        }
        Some(self.context.struct_type(&elems, false))
    }

    /// 8-byte words needed to roundtrip a value of this type through an
    /// `i64` word buffer (overestimates for sub-word scalars; padding
    /// roundtrips as garbage, consistently on store and load). `None` for
    /// types with no fixed size here (vectors).
    fn llvm_word_count(ty: &BasicTypeEnum<'ctx>) -> Option<u64> {
        match ty {
            BasicTypeEnum::IntType(it) => Some(((it.get_bit_width() as u64 + 63) / 64).max(1)),
            BasicTypeEnum::FloatType(_) => Some(2),
            BasicTypeEnum::PointerType(_) => Some(1),
            BasicTypeEnum::ArrayType(at) => Some(at.len() as u64 * Self::llvm_word_count(&at.get_element_type())?),
            BasicTypeEnum::StructType(st) => {
                let mut total = 0u64;
                for i in 0..st.count_fields() {
                    total += Self::llvm_word_count(&st.get_field_type_at_index(i).unwrap())?;
                }
                Some(total)
            }
            _ => None,
        }
    }

    /// Whether values of this LLVM type own heap pairs (directly or through
    /// struct fields, cycle-guarded). Word-copied payloads must not contain
    /// these: scope destruction would miss them (double-own on copy).
    fn type_has_own_pair(&self, ty: &BasicTypeEnum<'ctx>, visited: &mut HashSet<String>) -> bool {
        match ty {
            BasicTypeEnum::StructType(st) => {
                if self.pair_owner_of(*st).is_some() {
                    return true;
                }
                for (name, field_map) in &self.struct_fields {
                    if self.struct_types.get(name) != Some(st) {
                        continue;
                    }
                    if !visited.insert(name.clone()) {
                        continue;
                    }
                    for idx in field_map.values() {
                        if let Some(fty) = st.get_field_type_at_index(*idx) {
                            if self.type_has_own_pair(&fty, visited) {
                                return true;
                            }
                        }
                    }
                }
                false
            }
            BasicTypeEnum::ArrayType(at) => self.type_has_own_pair(&at.get_element_type(), visited),
            _ => false,
        }
    }

    /// Element LLVM type for a `vec` declaration type. `Any` (from `vec[]`
    /// with no established type) uses i64 slots; values convert at the
    /// `push`/use boundaries via [`Self::coerce_to_ty`].
    fn vec_elem_llvm_ty(&self, ty: &Type) -> BasicTypeEnum<'ctx> {
        match ty {
            Type::Vec { elem, .. } => match elem.as_ref() {
                Type::Any(_) => self.context.i64_type().into(),
                _ => self.llvm_ty_for(elem),
            },
            Type::Any(_) => self.context.i64_type().into(),
            _ => self.llvm_ty_for(ty),
        }
    }

    /// Is this variable a vector (tracked at declaration)?
    fn is_vec_var(&self, name: &str) -> bool {
        if self.vec_vars.contains(name) {
            return true;
        }
        let lookup = name.rsplit("::").next().unwrap_or(name);
        lookup != name && self.vec_vars.contains(lookup)
    }

    /// `true` when the AST type denotes an unsigned int (`u8..u128`,
    /// `uint`, resolved through the same stdlib-name table as sema).
    fn ast_ty_is_unsigned(ty: &Type) -> bool {
        match ty {
            Type::Named(name, _) => matches!(
                crate::sema::Ty::from_stdlib_name(name.rsplit("::").next().unwrap_or(name)),
                Some(crate::sema::Ty::SizedInt { signed: false, .. })
                    | Some(crate::sema::Ty::UInt)
            ),
            _ => false,
        }
    }

    /// `true` when the sema type is an unsigned int.
    fn sema_ty_is_unsigned(ty: &crate::sema::Ty) -> bool {
        matches!(
            ty,
            crate::sema::Ty::SizedInt { signed: false, .. }
                | crate::sema::Ty::UInt
        )
    }

    /// Record an unsigned-int variable for shift lowering. Call at every
    /// declaration site that tracks `string_vars`/`vec_vars`.
    fn track_unsigned_var(&mut self, name: &str, ty: &Type) {
        if Self::ast_ty_is_unsigned(ty) {
            self.unsigned_vars.insert(name.to_string());
        }
    }

    /// Is this variable an unsigned int (tracked at declaration)?
    fn is_unsigned_var(&self, name: &str) -> bool {
        if self.unsigned_vars.contains(name) {
            return true;
        }
        let lookup = name.rsplit("::").next().unwrap_or(name);
        lookup != name && self.unsigned_vars.contains(lookup)
    }

    /// `true` when `>>` on this expression must be a logical (zero-fill)
    /// shift: the operand is an unsigned int. Idents resolve through
    /// decl-site tracking (LLVM ints carry no signedness); calls resolve
    /// through the callee's declared return type; anything else falls
    /// back to `infer_expr_ty` (literals stay arithmetic).
    fn is_unsigned_expr(&self, expr: &Expr) -> bool {
        match &expr.kind {
            ExprKind::Paren(inner) => self.is_unsigned_expr(inner),
            ExprKind::Ident(name) => self.is_unsigned_var(name),
            ExprKind::Call { callee, .. } => self
                .funcs
                .get(callee.as_str())
                .is_some_and(|(_, info)| Self::sema_ty_is_unsigned(&info.ret)),
            _ => matches!(
                self.infer_expr_ty(expr),
                Ok(crate::sema::Ty::SizedInt { signed: false, .. })
                    | Ok(crate::sema::Ty::UInt)
            ),
        }
    }

    /// Map struct type `{ [CAP x K], [CAP x V], i64 len }`.
    fn map_struct_ty(
        &self,
        key: BasicTypeEnum<'ctx>,
        val: BasicTypeEnum<'ctx>,
    ) -> StructType<'ctx> {
        let keys: BasicTypeEnum<'ctx> = match key {
            BasicTypeEnum::IntType(it) => it.array_type(Self::MAP_CAP).into(),
            BasicTypeEnum::FloatType(ft) => ft.array_type(Self::MAP_CAP).into(),
            BasicTypeEnum::PointerType(pt) => pt.array_type(Self::MAP_CAP).into(),
            BasicTypeEnum::StructType(st) => st.array_type(Self::MAP_CAP).into(),
            BasicTypeEnum::ArrayType(at) => at.array_type(Self::MAP_CAP).into(),
            _ => self.context.i64_type().array_type(Self::MAP_CAP).into(),
        };
        let vals: BasicTypeEnum<'ctx> = match val {
            BasicTypeEnum::IntType(it) => it.array_type(Self::MAP_CAP).into(),
            BasicTypeEnum::FloatType(ft) => ft.array_type(Self::MAP_CAP).into(),
            BasicTypeEnum::PointerType(pt) => pt.array_type(Self::MAP_CAP).into(),
            BasicTypeEnum::StructType(st) => st.array_type(Self::MAP_CAP).into(),
            BasicTypeEnum::ArrayType(at) => at.array_type(Self::MAP_CAP).into(),
            _ => self.context.i64_type().array_type(Self::MAP_CAP).into(),
        };
        self.context.struct_type(
            &[keys.into(), vals.into(), self.context.i64_type().into()],
            false,
        )
    }

    /// Key/value LLVM types for a map declaration. `Any` sides (inferred
    /// `any m = has ... end`) fall back to the literal shape via
    /// [`Self::lit_slot_ty`] when entries are available, else i64/ptr.
    fn map_keyval_llvm_ty(&self, ty: &Type, entries: &[ (Expr, Expr) ]) -> (BasicTypeEnum<'ctx>, BasicTypeEnum<'ctx>) {
        match ty {
            Type::Map { key, value, .. } => {
                let k = match key.as_ref() {
                    Type::Any(_) => entries.first().map(|(k, _)| self.lit_slot_ty(k, true)).unwrap_or_else(|| self.context.i64_type().into()),
                    _ => self.llvm_ty_for(key),
                };
                let v = match value.as_ref() {
                    Type::Any(_) => entries.first().map(|(_, v)| self.lit_slot_ty(v, false)).unwrap_or_else(|| self.context.i64_type().into()),
                    _ => self.llvm_ty_for(value),
                };
                (k, v)
            }
            Type::Any(_) => {
                let k = entries.first().map(|(k, _)| self.lit_slot_ty(k, true)).unwrap_or_else(|| self.context.i64_type().into());
                let v = entries.first().map(|(_, v)| self.lit_slot_ty(v, false)).unwrap_or_else(|| self.context.i64_type().into());
                (k, v)
            }
            _ => (self.context.i64_type().into(), self.context.i64_type().into()),
        }
    }

    /// Slot type for a literal key/value by shape: strings → ptr, ints →
    /// i64 (widened at use), bools → i1, chars → i32, floats → f64.
    fn lit_slot_ty(&self, e: &Expr, _is_key: bool) -> BasicTypeEnum<'ctx> {
        match &e.kind {
            ExprKind::StringLit(_) => self.context.ptr_type(inkwell::AddressSpace::default()).into(),
            ExprKind::IntLit(_) => self.context.i64_type().into(),
            ExprKind::BoolLit(_) => self.context.bool_type().into(),
            ExprKind::CharLit(_) => self.context.i32_type().into(),
            ExprKind::FloatLit(_) => self.context.f64_type().into(),
            _ => self.context.i64_type().into(),
        }
    }

    fn get_or_declare_strcmp(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("strcmp") {
            return f;
        }
        let ptr = self.context.ptr_type(inkwell::AddressSpace::default());
        let fn_ty = self.context.i32_type().fn_type(&[ptr.into(), ptr.into()], false);
        self.module.add_function("strcmp", fn_ty, None)
    }

    /// Is this variable a map (tracked at declaration)?
    fn is_map_var(&self, name: &str) -> bool {
        if self.map_vars.contains(name) {
            return true;
        }
        let lookup = name.rsplit("::").next().unwrap_or(name);
        lookup != name && self.map_vars.contains(lookup)
    }

    /// Is this variable a string (tracked at declaration)?
    fn is_string_var(&self, name: &str) -> bool {
        if self.string_vars.contains(name) {
            return true;
        }
        let lookup = name.rsplit("::").next().unwrap_or(name);
        lookup != name && self.string_vars.contains(lookup)
    }

    fn get_or_declare_strlen(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("strlen") {
            return f;
        }
        let ptr = self.context.ptr_type(inkwell::AddressSpace::default());
        let fn_ty = self.context.i64_type().fn_type(&[ptr.into()], false);
        self.module.add_function("strlen", fn_ty, None)
    }

    /// Emit `if (!cond) abort()` in straight-line code and continue after.
    /// Used for empty `pop`/`first`/`last` and capacity overflow traps.
    fn codegen_trap_unless(
        &self,
        cond: inkwell::values::IntValue<'ctx>,
        span: Span,
    ) -> Result<(), CodegenError> {
        let func = self.cur_fn.ok_or(CodegenError{message: "trap outside function".into(), span})?;
        let ok_bb = self.context.append_basic_block(func, "trap.ok");
        let fail_bb = self.context.append_basic_block(func, "trap.fail");
        self.builder.build_conditional_branch(cond, ok_bb, fail_bb).unwrap();
        self.builder.position_at_end(fail_bb);
        self.builder.build_call(self.get_or_declare_abort(), &[], "trap.abort").unwrap();
        self.builder.build_unreachable().unwrap();
        self.builder.position_at_end(ok_bb);
        Ok(())
    }

    /// Search an array buffer `[0..len)` for `key`; returns i1 found.
    /// Integer slots compare directly (key coerced to slot width); pointer
    /// slots compare by content (`strcmp`).
    fn codegen_buffer_contains(
        &self,
        buf_ptr: PointerValue<'ctx>,
        arr_ty: inkwell::types::ArrayType<'ctx>,
        len: inkwell::values::IntValue<'ctx>,
        key_val: BasicValueEnum<'ctx>,
        span: Span,
    ) -> Result<inkwell::values::IntValue<'ctx>, CodegenError> {
        let slot_ty = arr_ty.get_element_type();
        let key = self.coerce_to_ty(key_val, slot_ty);
        let func = self.cur_fn.ok_or(CodegenError{message: "contains outside function".into(), span})?;
        let found_ptr = self.create_entry_block_alloca("contains.found", self.context.bool_type().into());
        self.builder.build_store(found_ptr, self.context.bool_type().const_zero()).unwrap();
        let i_ptr = self.create_entry_block_alloca("contains.i", self.context.i64_type().into());
        self.builder.build_store(i_ptr, self.context.i64_type().const_zero()).unwrap();
        let loop_bb = self.context.append_basic_block(func, "contains.loop");
        let body_bb = self.context.append_basic_block(func, "contains.body");
        let exit_bb = self.context.append_basic_block(func, "contains.exit");
        let zero = self.context.i64_type().const_int(0, false);
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(loop_bb);
        let i = self.builder.build_load(self.context.i64_type(), i_ptr, "contains.i").unwrap().into_int_value();
        let cont = self.builder.build_int_compare(IntPredicate::ULT, i, len, "contains.cont").unwrap();
        self.builder.build_conditional_branch(cont, body_bb, exit_bb).unwrap();
        self.builder.position_at_end(body_bb);
        let eptr = unsafe {
            self.builder.build_gep(arr_ty, buf_ptr, &[zero, i], "contains.slot").unwrap()
        };
        let slot = self.builder.build_load(slot_ty, eptr, "contains.elem").unwrap();
        let eq = match (slot.get_type(), key.get_type()) {
            (BasicTypeEnum::IntType(a), BasicTypeEnum::IntType(b)) if a.get_bit_width() == b.get_bit_width() => {
                self.builder.build_int_compare(IntPredicate::EQ, slot.into_int_value(), key.into_int_value(), "contains.eq").unwrap()
            }
            (BasicTypeEnum::PointerType(_), BasicTypeEnum::PointerType(_)) => {
                let cmp = self.builder.build_call(self.get_or_declare_strcmp(), &[slot.into(), key.into()], "contains.strcmp").unwrap();
                let c = cmp.try_as_basic_value().basic().unwrap().into_int_value();
                self.builder.build_int_compare(IntPredicate::EQ, c, self.context.i32_type().const_zero(), "contains.eq").unwrap()
            }
            _ => self.context.bool_type().const_int(0, false),
        };
        // found |= eq (no early exit; idempotent).
        let prev = self.builder.build_load(self.context.bool_type(), found_ptr, "contains.prev").unwrap().into_int_value();
        let both = self.builder.build_or(prev, eq, "contains.any").unwrap();
        self.builder.build_store(found_ptr, both).unwrap();
        let one = self.context.i64_type().const_int(1, false);
        let ni = self.builder.build_int_add(i, one, "contains.inc").unwrap();
        self.builder.build_store(i_ptr, ni).unwrap();
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(exit_bb);
        Ok(self.builder.build_load(self.context.bool_type(), found_ptr, "contains").unwrap().into_int_value())
    }

    /// Module global capturing argv[0] (the program name) in main's
    /// prologue. Created on demand, zero-init null (libraries without a
    /// main read null — the stdlib wrapper maps that to `""`).
    fn argv0_global(&self) -> PointerValue<'ctx> {
        if let Some(g) = self.module.get_global("__hella_argv0") {
            return g.as_pointer_value();
        }
        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
        let g = self.module.add_global(ptr_ty, None, "__hella_argv0");
        g.set_initializer(&ptr_ty.const_null());
        g.as_pointer_value()
    }

    /// True when `ty` is a lowered range value
    /// (`{i64 start, i64 end, i1 inclusive}`) as opposed to a vec/map whose
    /// first field is a buffer array.
    fn is_range_struct(ty: BasicTypeEnum<'ctx>) -> bool {
        if !ty.is_struct_type() {
            return false;
        }
        let st = ty.into_struct_type();
        if st.count_fields() != 3 {
            return false;
        }
        let f0_is_i64 = matches!(
            st.get_field_type_at_index(0).unwrap(),
            BasicTypeEnum::IntType(t) if t.get_bit_width() == 64
        );
        let f2_is_i1 = matches!(
            st.get_field_type_at_index(2).unwrap(),
            BasicTypeEnum::IntType(t) if t.get_bit_width() == 1
        );
        f0_is_i64 && f2_is_i1
    }

    /// True argument count for main's `args` array, loaded from the hidden
    /// `__hella_argc` slot — or `None` when `ptr` is any other array.
    fn main_args_len(&self, ptr: PointerValue<'ctx>) -> Option<inkwell::values::IntValue<'ctx>> {
        if self.main_args_alloca == Some(ptr) {
            self.main_argc_alloca.map(|a| {
                self.builder
                    .build_load(self.context.i64_type(), a, "args.len")
                    .unwrap()
                    .into_int_value()
            })
        } else {
            None
        }
    }

    /// Collection and string methods (`len`, `is_empty`, `pop`, `clear`,
    /// `contains`, `first`, `last`, `remove`, `get`; `push` is separate).
    /// Sema has validated arity and types. Returns `Ok(None)` when the base
    /// is not a tracked collection/string, falling through to class methods.
    /// Bases are plain identifiers (MVP).
    fn codegen_collection_method(
        &mut self,
        name: &str,
        method: &str,
        args: &[CallArg],
        span: Span,
    ) -> Result<Option<BasicValueEnum<'ctx>>, CodegenError> {
        enum Family {
            Vec,
            Arr,
            Map,
            Str,
        }
        let (ptr, ty) = match self.lookup_var(name) {
            Some(v) => v,
            None => return Ok(None),
        };
        let family = if self.is_vec_var(name) && ty.is_struct_type() {
            Family::Vec
        } else if self.is_map_var(name) && ty.is_struct_type() {
            Family::Map
        } else if ty.is_array_type() {
            Family::Arr
        } else if self.is_string_var(name) && ty.is_pointer_type() {
            Family::Str
        } else {
            return Ok(None);
        };
        // Reject unknown methods per family here (sema already diagnosed).
        let known = match family {
            Family::Vec => matches!(method, "len" | "is_empty" | "pop" | "clear" | "contains" | "first" | "last"),
            Family::Arr => matches!(method, "len" | "is_empty" | "contains" | "first" | "last"),
            Family::Map => matches!(method, "len" | "is_empty" | "contains" | "remove" | "clear" | "get_or"),
            Family::Str => matches!(method, "len" | "is_empty"),
        };
        if !known {
            return Err(CodegenError{message: format!("unknown method `{method}` for `{name}`"), span});
        }
        let zero64 = self.context.i64_type().const_int(0, false);
        let one64 = self.context.i64_type().const_int(1, false);
        // Resolve buffer/length accessors per family.
        enum Buf<'ctx> {
            Vec { st: StructType<'ctx>, arr: inkwell::types::ArrayType<'ctx>, len: inkwell::values::IntValue<'ctx> },
            // Fixed arrays use the static size — except main's `args`,
            // which carries the true argc in `dyn_len`.
            Arr { arr: inkwell::types::ArrayType<'ctx>, n: u64, dyn_len: Option<inkwell::values::IntValue<'ctx>> },
            Map { st: StructType<'ctx>, keys: inkwell::types::ArrayType<'ctx>, vals: inkwell::types::ArrayType<'ctx>, len: inkwell::values::IntValue<'ctx> },
            Str { val: PointerValue<'ctx> },
        }
        let buf = match family {
            Family::Vec => {
                let st = ty.into_struct_type();
                let buf_ptr = self.builder.build_struct_gep(st, ptr, 0, "m.vec.buf").unwrap();
                let arr = match st.get_field_type_at_index(0).unwrap() {
                    BasicTypeEnum::ArrayType(at) => at,
                    _ => return Err(CodegenError{message: "malformed vector buffer".into(), span}),
                };
                let len_ptr = self.builder.build_struct_gep(st, ptr, 1, "m.vec.len").unwrap();
                let len = self.builder.build_load(self.context.i64_type(), len_ptr, "m.vec.len").unwrap().into_int_value();
                let _ = buf_ptr;
                Buf::Vec { st, arr, len }
            }
            Family::Arr => {
                let arr = ty.into_array_type();
                Buf::Arr { arr, n: arr.len() as u64, dyn_len: self.main_args_len(ptr) }
            }
            Family::Map => {
                let st = ty.into_struct_type();
                let keys = match st.get_field_type_at_index(0).unwrap() {
                    BasicTypeEnum::ArrayType(at) => at,
                    _ => return Err(CodegenError{message: "malformed map keys buffer".into(), span}),
                };
                let vals = match st.get_field_type_at_index(1).unwrap() {
                    BasicTypeEnum::ArrayType(at) => at,
                    _ => return Err(CodegenError{message: "malformed map values buffer".into(), span}),
                };
                let len_ptr = self.builder.build_struct_gep(st, ptr, 2, "m.map.len").unwrap();
                let len = self.builder.build_load(self.context.i64_type(), len_ptr, "m.map.len").unwrap().into_int_value();
                Buf::Map { st, keys, vals, len }
            }
            Family::Str => {
                let val = self.builder.build_load(ty, ptr, "m.str").unwrap().into_pointer_value();
                Buf::Str { val }
            }
        };
        // Buffer pointer + element type for Vec/Arr families.
        match method {
            "len" => {
                let v: BasicValueEnum<'ctx> = match &buf {
                    Buf::Vec { len, .. } | Buf::Map { len, .. } => (*len).into(),
                    Buf::Arr { dyn_len: Some(l), .. } => (*l).into(),
                    Buf::Arr { n, .. } => self.context.i64_type().const_int(*n, false).into(),
                    Buf::Str { val } => {
                        let call = self.builder.build_call(self.get_or_declare_strlen(), &[(*val).into()], "m.strlen").unwrap();
                        call.try_as_basic_value().basic().unwrap()
                    }
                };
                Ok(Some(v))
            }
            "is_empty" => {
                let is0 = match &buf {
                    Buf::Vec { len, .. } | Buf::Map { len, .. } => {
                        self.builder.build_int_compare(IntPredicate::EQ, *len, zero64, "m.empty").unwrap()
                    }
                    Buf::Arr { dyn_len: Some(l), .. } => {
                        self.builder.build_int_compare(IntPredicate::EQ, *l, zero64, "m.empty").unwrap()
                    }
                    Buf::Arr { n, .. } => self.context.bool_type().const_int(if *n == 0 { 1 } else { 0 }, false),
                    Buf::Str { val } => {
                        let call = self.builder.build_call(self.get_or_declare_strlen(), &[(*val).into()], "m.strlen").unwrap();
                        let l = call.try_as_basic_value().basic().unwrap().into_int_value();
                        self.builder.build_int_compare(IntPredicate::EQ, l, zero64, "m.empty").unwrap()
                    }
                };
                Ok(Some(is0.into()))
            }
            "pop" => {
                // Vectors only (sema enforced).
                let (st, arr, len) = match &buf {
                    Buf::Vec { st, arr, len } => (*st, *arr, *len),
                    _ => return Err(CodegenError{message: "`pop` needs a vector".into(), span}),
                };
                let nonzero = self.builder.build_int_compare(IntPredicate::NE, len, zero64, "m.pop.nonempty").unwrap();
                self.codegen_trap_unless(nonzero, span)?;
                let nlen = self.builder.build_int_sub(len, one64, "m.pop.dec").unwrap();
                let len_ptr = self.builder.build_struct_gep(st, ptr, 1, "m.pop.len").unwrap();
                self.builder.build_store(len_ptr, nlen).unwrap();
                let buf_ptr = self.builder.build_struct_gep(st, ptr, 0, "m.pop.buf").unwrap();
                let eptr = unsafe {
                    self.builder.build_gep(arr, buf_ptr, &[zero64, nlen], "m.pop.slot").unwrap()
                };
                let elem_ty = arr.get_element_type();
                let loaded = self.builder.build_load(elem_ty, eptr, "m.pop").unwrap();
                // Null the popped slot so a stale pair (for `own` element types)
                // is never left with a live data pointer behind the decreased len.
                self.null_own_fields(eptr, elem_ty, 0);
                Ok(Some(loaded))
            }
            "clear" => {
                // Destroy all live elements before clearing so `own` content is
                // freed (mirrors the generated `__container_dtor_N` walk).
                match &buf {
                    Buf::Vec { st, arr, len } => {
                        let func = self.cur_fn.ok_or(CodegenError { message: "`clear` outside function".into(), span })?;
                        let destroy_bb = self.context.append_basic_block(func, "clear.vec.destroy");
                        let done_bb = self.context.append_basic_block(func, "clear.vec.done");
                        self.builder.build_unconditional_branch(destroy_bb).unwrap();
                        self.builder.position_at_end(destroy_bb);
                        let i64_ty = self.context.i64_type();
                        let idx_ptr = self.create_entry_block_alloca("clear.idx", i64_ty.into());
                        self.builder.build_store(idx_ptr, i64_ty.const_zero()).unwrap();
                        let cond_bb = self.context.append_basic_block(func, "clear.vec.cond");
                        self.builder.build_unconditional_branch(cond_bb).unwrap();
                        self.builder.position_at_end(cond_bb);
                        let idx = self.builder.build_load(i64_ty, idx_ptr, "clear.idx").unwrap().into_int_value();
                        let more = self.builder.build_int_compare(IntPredicate::SLT, idx, *len, "clear.vec.more").unwrap();
                        self.builder.build_conditional_branch(more, destroy_bb, done_bb).unwrap();
                        self.builder.position_at_end(destroy_bb);
                        let buf_ptr = self.builder.build_struct_gep(*st, ptr, 0, "clear.buf.ptr").unwrap();
                        let eptr = unsafe {
                            self.builder
                                .build_gep(*arr, buf_ptr, &[i64_ty.const_zero(), idx], "clear.elem")
                                .unwrap()
                        };
                        self.emit_field_destroy_for_ty(eptr, (*arr).get_element_type(), 0);
                        let next = self.builder.build_int_add(idx, i64_ty.const_int(1, false), "clear.next").unwrap();
                        self.builder.build_store(idx_ptr, next).unwrap();
                        self.builder.build_unconditional_branch(cond_bb).unwrap();
                        self.builder.position_at_end(done_bb);
                        let len_ptr = self.builder.build_struct_gep(*st, ptr, 1, "m.clear.len").unwrap();
                        self.builder.build_store(len_ptr, zero64).unwrap();
                    }
                    Buf::Map { st, keys, vals, len } => {
                        let func = self.cur_fn.ok_or(CodegenError { message: "`clear` outside function".into(), span })?;
                        let destroy_bb = self.context.append_basic_block(func, "clear.map.destroy");
                        let done_bb = self.context.append_basic_block(func, "clear.map.done");
                        self.builder.build_unconditional_branch(destroy_bb).unwrap();
                        self.builder.position_at_end(destroy_bb);
                        let i64_ty = self.context.i64_type();
                        let idx_ptr = self.create_entry_block_alloca("clear.idx", i64_ty.into());
                        self.builder.build_store(idx_ptr, i64_ty.const_zero()).unwrap();
                        let cond_bb = self.context.append_basic_block(func, "clear.map.cond");
                        self.builder.build_unconditional_branch(cond_bb).unwrap();
                        self.builder.position_at_end(cond_bb);
                        let idx = self.builder.build_load(i64_ty, idx_ptr, "clear.idx").unwrap().into_int_value();
                        let more = self.builder.build_int_compare(IntPredicate::SLT, idx, *len, "clear.map.more").unwrap();
                        self.builder.build_conditional_branch(more, destroy_bb, done_bb).unwrap();
                        self.builder.position_at_end(destroy_bb);
                        let keys_ptr = self.builder.build_struct_gep(*st, ptr, 0, "clear.keys.ptr").unwrap();
                        let vals_ptr = self.builder.build_struct_gep(*st, ptr, 1, "clear.vals.ptr").unwrap();
                        let kptr = unsafe {
                            self.builder
                                .build_gep(*keys, keys_ptr, &[i64_ty.const_zero(), idx], "clear.key")
                                .unwrap()
                        };
                        self.emit_field_destroy_for_ty(kptr, (*keys).get_element_type(), 0);
                        let vptr = unsafe {
                            self.builder
                                .build_gep(*vals, vals_ptr, &[i64_ty.const_zero(), idx], "clear.val")
                                .unwrap()
                        };
                        self.emit_field_destroy_for_ty(vptr, (*vals).get_element_type(), 0);
                        let next = self.builder.build_int_add(idx, i64_ty.const_int(1, false), "clear.next").unwrap();
                        self.builder.build_store(idx_ptr, next).unwrap();
                        self.builder.build_unconditional_branch(cond_bb).unwrap();
                        self.builder.position_at_end(done_bb);
                        let len_ptr = self.builder.build_struct_gep(*st, ptr, 2, "m.clear.len").unwrap();
                        self.builder.build_store(len_ptr, zero64).unwrap();
                    }
                    _ => return Err(CodegenError{message: "`clear` needs a vector or map".into(), span}),
                }
                Ok(Some(self.context.i64_type().const_int(0, false).into()))
            }
            "contains" => {
                let arg_val = self.codegen_call_arg(&args[0])?;
                let found = match &buf {
                    Buf::Vec { st, arr, len } => {
                        let buf_ptr = self.builder.build_struct_gep(*st, ptr, 0, "m.contains.buf").unwrap();
                        self.codegen_buffer_contains(buf_ptr, *arr, *len, arg_val, span)?
                    }
                    Buf::Arr { arr, n, dyn_len } => {
                        // Dynamic argc bound for main's `args` (skips the
                        // null padding — and a potential strcmp(null)); the
                        // static size for every other array.
                        let nlen = match dyn_len {
                            Some(l) => (*l).into(),
                            None => self.context.i64_type().const_int(*n, false).into(),
                        };
                        self.codegen_buffer_contains(ptr, *arr, nlen, arg_val, span)?
                    }
                    Buf::Map { st, keys, len, .. } => {
                        let keys_ptr = self.builder.build_struct_gep(*st, ptr, 0, "m.contains.keys").unwrap();
                        // Reuse the search loop shape via buffer scan.
                        self.codegen_buffer_contains(keys_ptr, *keys, *len, arg_val, span)?
                    }
                    Buf::Str { .. } => return Err(CodegenError{message: "`contains` needs a collection".into(), span}),
                };
                Ok(Some(found.into()))
            }
            "first" | "last" => {
                let is_first = method == "first";
                let (bp, arr, len_v): (PointerValue<'ctx>, inkwell::types::ArrayType<'ctx>, Option<inkwell::values::IntValue<'ctx>>) = match &buf {
                    Buf::Vec { st, arr, len } => {
                        let buf_ptr = self.builder.build_struct_gep(*st, ptr, 0, "m.edge.buf").unwrap();
                        (buf_ptr, *arr, Some(*len))
                    }
                    Buf::Arr { arr, n, dyn_len } => {
                        match dyn_len {
                            // main's `args`: `last` is the final real
                            // argument (traps when empty); `first` is slot 0.
                            Some(l) => (ptr, *arr, Some(*l)),
                            None => {
                                if *n == 0 {
                                    self.codegen_trap_unless(self.context.bool_type().const_int(0, false), span)?;
                                }
                                (ptr, *arr, None)
                            }
                        }
                    }
                    _ => return Err(CodegenError{message: format!("`{method}` needs an array or vector"), span}),
                };
                let idx = if is_first {
                    zero64
                } else {
                    match len_v {
                        Some(len) => {
                            let nonzero = self.builder.build_int_compare(IntPredicate::NE, len, zero64, "m.last.nonempty").unwrap();
                            self.codegen_trap_unless(nonzero, span)?;
                            self.builder.build_int_sub(len, one64, "m.last.idx").unwrap()
                        }
                        None => {
                            // Static array: N > 0 checked above.
                            let n = arr.len() as u64;
                            self.context.i64_type().const_int(n - 1, false)
                        }
                    }
                };
                let eptr = unsafe {
                    self.builder.build_gep(arr, bp, &[zero64, idx], "m.edge.slot").unwrap()
                };
                let elem_ty = arr.get_element_type();
                Ok(Some(self.builder.build_load(elem_ty, eptr, "m.edge").unwrap()))
            }
            "remove" => {
                // Maps only (sema enforced). Swap-with-last + shrink.
                let (st, keys, vals, len) = match &buf {
                    Buf::Map { st, keys, vals, len } => (*st, *keys, *vals, *len),
                    _ => return Err(CodegenError{message: "`remove` needs a map".into(), span}),
                };
                let key_val = self.codegen_call_arg(&args[0])?;
                let (idx_res, _, _, _) = self.codegen_map_search(ptr, st, key_val, span)?;
                let func = self.cur_fn.ok_or(CodegenError{message: "map access outside function".into(), span})?;
                let hit_bb = self.context.append_basic_block(func, "map.rm.hit");
                let miss_bb = self.context.append_basic_block(func, "map.rm.miss");
                let merge_bb = self.context.append_basic_block(func, "map.rm.merge");
                let res = self.create_entry_block_alloca("map.rm.res", self.context.bool_type().into());
                self.builder.build_store(res, self.context.bool_type().const_zero()).unwrap();
                let idx = self.builder.build_load(self.context.i64_type(), idx_res, "map.rm.idx").unwrap().into_int_value();
                let is_hit = self.builder.build_int_compare(IntPredicate::SGE, idx, zero64, "map.rm.found").unwrap();
                self.builder.build_conditional_branch(is_hit, hit_bb, miss_bb).unwrap();
                // hit: move last entry into idx, shrink.
                self.builder.position_at_end(hit_bb);
                let len_ptr = self.builder.build_struct_gep(st, ptr, 2, "map.rm.len").unwrap();
                let nlen = self.builder.build_int_sub(len, one64, "map.rm.dec").unwrap();
                let keys_ptr = self.builder.build_struct_gep(st, ptr, 0, "map.rm.keys").unwrap();
                let vals_ptr = self.builder.build_struct_gep(st, ptr, 1, "map.rm.vals").unwrap();
                let zero = zero64;
                let last_kptr = unsafe { self.builder.build_gep(keys, keys_ptr, &[zero, nlen], "map.rm.lastk").unwrap() };
                let last_vptr = unsafe { self.builder.build_gep(vals, vals_ptr, &[zero, nlen], "map.rm.lastv").unwrap() };
                let lk = self.builder.build_load(keys.get_element_type(), last_kptr, "map.rm.lk").unwrap();
                let lv = self.builder.build_load(vals.get_element_type(), last_vptr, "map.rm.lv").unwrap();
                let dst_kptr = unsafe { self.builder.build_gep(keys, keys_ptr, &[zero, idx], "map.rm.dstk").unwrap() };
                let dst_vptr = unsafe { self.builder.build_gep(vals, vals_ptr, &[zero, idx], "map.rm.dstv").unwrap() };
                self.builder.build_store(dst_kptr, lk).unwrap();
                self.builder.build_store(dst_vptr, lv).unwrap();
                self.builder.build_store(len_ptr, nlen).unwrap();
                self.builder.build_store(res, self.context.bool_type().const_int(1, false)).unwrap();
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                self.builder.position_at_end(miss_bb);
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                self.builder.position_at_end(merge_bb);
                Ok(Some(self.builder.build_load(self.context.bool_type(), res, "map.rm").unwrap()))
            }
            "get_or" => {
                // Maps only (sema enforced): hit ? vals[idx] : default.
                let (st, vals, len) = match &buf {
                    Buf::Map { st, vals, len, .. } => (*st, *vals, *len),
                    _ => return Err(CodegenError{message: "`get` needs a map".into(), span}),
                };
                let _ = len;
                let key_val = self.codegen_call_arg(&args[0])?;
                let dflt_val = self.codegen_call_arg(&args[1])?;
                let (idx_res, _, vals_arr_ty, val_ty) = self.codegen_map_search(ptr, st, key_val, span)?;
                let func = self.cur_fn.ok_or(CodegenError{message: "map access outside function".into(), span})?;
                let hit_bb = self.context.append_basic_block(func, "map.get2.hit");
                let miss_bb = self.context.append_basic_block(func, "map.get2.miss");
                let merge_bb = self.context.append_basic_block(func, "map.get2.merge");
                let res = self.create_entry_block_alloca("map.get2.res", val_ty);
                let cd = self.coerce_to_ty(dflt_val, val_ty);
                self.builder.build_store(res, cd).unwrap();
                let idx = self.builder.build_load(self.context.i64_type(), idx_res, "map.get2.idx").unwrap().into_int_value();
                let is_hit = self.builder.build_int_compare(IntPredicate::SGE, idx, zero64, "map.get2.found").unwrap();
                self.builder.build_conditional_branch(is_hit, hit_bb, miss_bb).unwrap();
                self.builder.position_at_end(hit_bb);
                let vals_ptr = self.builder.build_struct_gep(st, ptr, 1, "map.get2.vals").unwrap();
                let vptr = unsafe {
                    self.builder.build_gep(vals_arr_ty, vals_ptr, &[zero64, idx], "map.get2.slot").unwrap()
                };
                let vv = self.builder.build_load(val_ty, vptr, "map.get2.val").unwrap();
                self.builder.build_store(res, vv).unwrap();
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                self.builder.position_at_end(miss_bb);
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                self.builder.position_at_end(merge_bb);
                Ok(Some(self.builder.build_load(val_ty, res, "map.get2").unwrap()))
            }
            _ => Err(CodegenError{message: format!("unknown method `{method}`"), span}),
        }
    }

    /// Search a map's keys for `key`, storing the matched slot index (or -1)    /// into a fresh i64 alloca. String slots compare by content (`strcmp`);
    /// integer slots compare directly (keys coerced to slot width first).
    /// Returns the index alloca plus buffer/val types. The builder is left at
    /// a fresh `exit` block.
    fn codegen_map_search(
        &mut self,
        map_ptr: PointerValue<'ctx>,
        map_st: StructType<'ctx>,
        key_val: BasicValueEnum<'ctx>,
        span: Span,
    ) -> Result<
        (
            PointerValue<'ctx>,
            inkwell::types::ArrayType<'ctx>,
            inkwell::types::ArrayType<'ctx>,
            BasicTypeEnum<'ctx>,
        ),
        CodegenError,
    > {
        let keys_arr_ty = match map_st.get_field_type_at_index(0).unwrap() {
            BasicTypeEnum::ArrayType(at) => at,
            _ => return Err(CodegenError{message: "malformed map keys buffer".into(), span}),
        };
        let vals_arr_ty = match map_st.get_field_type_at_index(1).unwrap() {
            BasicTypeEnum::ArrayType(at) => at,
            _ => return Err(CodegenError{message: "malformed map values buffer".into(), span}),
        };
        let key_slot_ty = keys_arr_ty.get_element_type();
        let val_ty = vals_arr_ty.get_element_type();
        let key = self.coerce_to_ty(key_val, key_slot_ty);
        let len_ptr = self.builder.build_struct_gep(map_st, map_ptr, 2, "map.len.ptr").unwrap();
        let len = self.builder.build_load(self.context.i64_type(), len_ptr, "map.len").unwrap().into_int_value();
        let func = self.cur_fn.ok_or(CodegenError{message: "map access outside function".into(), span})?;
        let idx_res = self.create_entry_block_alloca("map.search.idx", self.context.i64_type().into());
        self.builder.build_store(idx_res, self.context.i64_type().const_int(-1i64 as u64, false)).unwrap();
        let i_ptr = self.create_entry_block_alloca("map.search.i", self.context.i64_type().into());
        self.builder.build_store(i_ptr, self.context.i64_type().const_zero()).unwrap();
        let loop_bb = self.context.append_basic_block(func, "map.search.loop");
        let body_bb = self.context.append_basic_block(func, "map.search.body");
        let hit_bb = self.context.append_basic_block(func, "map.search.hit");
        let next_bb = self.context.append_basic_block(func, "map.search.next");
        let exit_bb = self.context.append_basic_block(func, "map.search.exit");
        let zero = self.context.i64_type().const_int(0, false);
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        // loop: i < len ?
        self.builder.position_at_end(loop_bb);
        let i = self.builder.build_load(self.context.i64_type(), i_ptr, "map.i").unwrap().into_int_value();
        let cont = self.builder.build_int_compare(IntPredicate::ULT, i, len, "map.cont").unwrap();
        self.builder.build_conditional_branch(cont, body_bb, exit_bb).unwrap();
        // body: compare keys[i]
        self.builder.position_at_end(body_bb);
        let kptr = unsafe {
            self.builder.build_gep(keys_arr_ty, self.builder.build_struct_gep(map_st, map_ptr, 0, "map.keys.ptr").unwrap(), &[zero, i], "map.key.ptr").unwrap()
        };
        let slot = self.builder.build_load(key_slot_ty, kptr, "map.key").unwrap();
        let eq = match (slot.get_type(), key.get_type()) {
            (BasicTypeEnum::IntType(a), BasicTypeEnum::IntType(b)) if a.get_bit_width() == b.get_bit_width() => {
                self.builder.build_int_compare(IntPredicate::EQ, slot.into_int_value(), key.into_int_value(), "map.key.eq").unwrap()
            }
            (BasicTypeEnum::PointerType(_), BasicTypeEnum::PointerType(_)) => {
                let cmp = self.builder.build_call(self.get_or_declare_strcmp(), &[slot.into(), key.into()], "map.strcmp").unwrap();
                let c = cmp.try_as_basic_value().basic().unwrap().into_int_value();
                self.builder.build_int_compare(IntPredicate::EQ, c, self.context.i32_type().const_zero(), "map.key.eq").unwrap()
            }
            _ => self.context.bool_type().const_int(0, false),
        };
        self.builder.build_conditional_branch(eq, hit_bb, next_bb).unwrap();
        // hit: record index, done
        self.builder.position_at_end(hit_bb);
        self.builder.build_store(idx_res, i).unwrap();
        self.builder.build_unconditional_branch(exit_bb).unwrap();
        // next: i += 1
        self.builder.position_at_end(next_bb);
        let one = self.context.i64_type().const_int(1, false);
        let ni = self.builder.build_int_add(i, one, "map.i.inc").unwrap();
        self.builder.build_store(i_ptr, ni).unwrap();
        self.builder.build_unconditional_branch(loop_bb).unwrap();
        self.builder.position_at_end(exit_bb);
        Ok((idx_res, keys_arr_ty, vals_arr_ty, val_ty))
    }

    /// Store map literal entries into an allocated map struct: keys into
    /// field 0, values into field 1 (both coerced), length into field 2.
    fn store_map_entries(
        &mut self,
        alloca: PointerValue<'ctx>,
        map_st: StructType<'ctx>,
        key_ty: BasicTypeEnum<'ctx>,
        val_ty: BasicTypeEnum<'ctx>,
        entries: &[(Expr, Expr)],
    ) -> Result<(), CodegenError> {
        let keys_ptr = self.builder.build_struct_gep(map_st, alloca, 0, "map.keys").unwrap();
        let vals_ptr = self.builder.build_struct_gep(map_st, alloca, 1, "map.vals").unwrap();
        let keys_arr_ty = match map_st.get_field_type_at_index(0).unwrap() {
            BasicTypeEnum::ArrayType(at) => at,
            _ => return Err(CodegenError{message: "malformed map keys buffer".into(), span: Span::new(0, 0)}),
        };
        let vals_arr_ty = match map_st.get_field_type_at_index(1).unwrap() {
            BasicTypeEnum::ArrayType(at) => at,
            _ => return Err(CodegenError{message: "malformed map values buffer".into(), span: Span::new(0, 0)}),
        };
        let zero = self.context.i64_type().const_int(0, false);
        for (i, (k, v)) in entries.iter().enumerate() {
            let kv = self.codegen_expr(k)?;
            let ck = self.coerce_to_ty(kv, key_ty);
            let idx = self.context.i64_type().const_int(i as u64, false);
            let kptr = unsafe {
                self.builder
                    .build_gep(keys_arr_ty, keys_ptr, &[zero, idx], &format!("map.key.{i}"))
                    .unwrap()
            };
            self.builder.build_store(kptr, ck).unwrap();
            let vv = self.codegen_expr(v)?;
            let cv = self.coerce_to_ty(vv, val_ty);
            let vptr = unsafe {
                self.builder
                    .build_gep(vals_arr_ty, vals_ptr, &[zero, idx], &format!("map.val.{i}"))
                    .unwrap()
            };
            self.builder.build_store(vptr, cv).unwrap();
        }
        let len_ptr = self.builder.build_struct_gep(map_st, alloca, 2, "map.len").unwrap();
        self.builder.build_store(len_ptr, self.context.i64_type().const_int(entries.len() as u64, false)).unwrap();
        Ok(())
    }

    fn llvm_ty_for(&self, ty: &Type) -> BasicTypeEnum<'ctx> {
        match ty {
            Type::Int(_) => self.context.i64_type().into(),
            Type::Bool(_) => self.context.bool_type().into(),
            Type::Char(_) => self.context.i32_type().into(),
            Type::String(_) => self
                .context
                .ptr_type(inkwell::AddressSpace::default())
                .into(),
            Type::Float(_) => self.context.f32_type().into(),
            Type::Double(_) => self.context.f64_type().into(),
            Type::Void(_) => {
                panic!("void not a first-class type in llvm_ty_for")
            }
            Type::Named(n, _) => {
                if n == "__derived__" {
                    return self.context.i64_type().into();
                }
                let lookup = n.rsplit("::").next().unwrap_or(n);
                // Implicit stdlib ints (types skill §1-2): `i8`..`u128`, `uint`
                // lex as Ident — lower directly without a struct lookup.
                if let Some(std_ty) = crate::sema::Ty::from_stdlib_name(lookup) {
                    match std_ty {
                        crate::sema::Ty::UInt | crate::sema::Ty::Int => {
                            return self.context.i64_type().into()
                        }
                        crate::sema::Ty::SizedInt { bits, .. } => {
                            return self.llvm_int_for_bits(bits).into()
                        }
                        _ => {}
                    }
                }
                if let Some(st) = self.struct_types.get(lookup) {
                    st.as_basic_type_enum().into()
                } else if let Some(et) = self.enum_types.get(lookup) {
                    et.as_basic_type_enum().into()
                } else if let Some(pair) = self.trait_pair_of(lookup) {
                    // Trait-typed slots lower to `{data ptr, type tag}` pairs.
                    pair.as_basic_type_enum().into()
                } else if lookup.len() == 1 && lookup.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false) {
                    // Unresolved single-uppercase name: a generic parameter
                    // (erasure MVP, mirrors sema). Known types resolve above.
                    self.context.i64_type().into()
                } else {
                    panic!("unknown struct/enum type {n}")
                }
            }
            Type::Generic(n, args, _) => {
                let lookup = n.rsplit("::").next().unwrap_or(n);
                if let Some(st) = self.struct_types.get(lookup) {
                    st.as_basic_type_enum().into()
                } else if let Some(et) = self.enum_types.get(lookup) {
                    et.as_basic_type_enum().into()
                } else {
                    let key = format!("{}<{}>", lookup, args.iter().map(|a| a.name()).collect::<Vec<_>>().join(","));
                    if let Some(st) = self.struct_types.get(&key) {
                        st.as_basic_type_enum().into()
                    } else {
                        panic!("unknown generic type {n}")
                    }
                }
            }
            Type::FunctionType(_, _, _) => self.context.ptr_type(inkwell::AddressSpace::default()).into(),
            Type::Tuple(tys, _) => {
                let tys_llvm: Vec<BasicTypeEnum> = tys.iter().map(|ty| self.llvm_ty_for(ty)).collect();
                self.context.struct_type(&tys_llvm, false).into()
            }
            Type::Any(_) => self.context.ptr_type(inkwell::AddressSpace::default()).into(),
            Type::Array(el, _) => {
                let inner = self.llvm_ty_for(el);
                match inner {
                    BasicTypeEnum::IntType(it) => it.array_type(16).into(),
                    BasicTypeEnum::PointerType(pt) => pt.array_type(16).into(),
                    BasicTypeEnum::FloatType(ft) => ft.array_type(16).into(),
                    BasicTypeEnum::StructType(st) => st.array_type(16).into(),
                    BasicTypeEnum::ArrayType(at) => at.array_type(16).into(),
                    _ => self.context.i64_type().array_type(16).into(),
                }
            }
            Type::FixedArray { elem, size, .. } => {
                // Explicit size → [N x elem]; inferred (None) → [16 x elem]
                // placeholder (locals with initializers refine at the decl site).
                let n = size.unwrap_or(16) as u32;
                let inner = self.llvm_ty_for(elem);
                match inner {
                    BasicTypeEnum::IntType(it) => it.array_type(n).into(),
                    BasicTypeEnum::PointerType(pt) => pt.array_type(n).into(),
                    BasicTypeEnum::FloatType(ft) => ft.array_type(n).into(),
                    BasicTypeEnum::StructType(st) => st.array_type(n).into(),
                    BasicTypeEnum::ArrayType(at) => at.array_type(n).into(),
                    _ => self.context.i64_type().array_type(n).into(),
                }
            }
            Type::Vec { .. } => {
                let elem = self.vec_elem_llvm_ty(ty);
                self.vec_struct_ty(elem).into()
            }
            Type::Map { key, value, .. } => {
                // `Any` sides without literal context: int keys, ptr values.
                let k = match key.as_ref() {
                    Type::Any(_) => self.context.i64_type().into(),
                    _ => self.llvm_ty_for(key),
                };
                let v = match value.as_ref() {
                    Type::Any(_) => self.context.ptr_type(inkwell::AddressSpace::default()).into(),
                    _ => self.llvm_ty_for(value),
                };
                self.map_struct_ty(k, v).into()
            }
            Type::Pointer(_, _) => self
                .context
                .ptr_type(inkwell::AddressSpace::default())
                .into(),
            // `task<T>` (Async-6): lowered as an opaque handle pointer
            // (the runtime's `hella_task_t*`). Sema knows `T`; LLVM only
            // ever passes the handle by pointer.
            Type::Task(_, _) => self
                .context
                .ptr_type(inkwell::AddressSpace::default())
                .into(),
            Type::Own(inner, _) => {
                // Every `own` slot is a pair (uniform with trait objects).
                let lookup = match inner.as_ref() {
                    Type::Named(n, _) => n.rsplit("::").next().unwrap_or(n).to_string(),
                    _ => String::new(),
                };
                match self.pair_types.get(&lookup) {
                    Some(pair) => pair.as_basic_type_enum().into(),
                    None => panic!("no pair type for own slot"),
                }
            }
            Type::Optional(el, _) => {
                let inner = self.llvm_ty_for(el);
                self.context
                    .struct_type(
                        &[inner.into(), self.context.bool_type().into()],
                        false,
                    )
                    .into()
            }
        }
    }

    fn llvm_ty_for_sema(
        &self,
        ty: &crate::sema::Ty,
    ) -> Option<BasicTypeEnum<'ctx>> {
        match ty {
            crate::sema::Ty::Int => Some(self.context.i64_type().into()),
            // types skill §2-3: `uint` is unsigned pointer-sized → i64 widths;
            // fixed widths lower to matching LLVM int types (signless in LLVM).
            crate::sema::Ty::UInt => Some(self.context.i64_type().into()),
            crate::sema::Ty::SizedInt { bits, .. } => Some(self.llvm_int_for_bits(*bits).into()),
            crate::sema::Ty::Bool => Some(self.context.bool_type().into()),
            crate::sema::Ty::Char => Some(self.context.i32_type().into()),
            crate::sema::Ty::String => Some(
                self.context
                    .ptr_type(inkwell::AddressSpace::default())
                    .into(),
            ),
            crate::sema::Ty::Void => None,
            crate::sema::Ty::Struct(n) => {
                if n == "__derived__" {
                    return Some(self.context.i64_type().into());
                }
                let lookup = n.rsplit("::").next().unwrap_or(n);
                if let Some(st) = self.struct_types.get(lookup) {
                    Some(st.as_basic_type_enum().into())
                } else if let Some(et) = self.enum_types.get(lookup) {
                    Some(et.as_basic_type_enum().into())
                } else if let Some(pair) = self.trait_pair_of(lookup) {
                    // Trait-typed slots lower to `{data ptr, type tag}` pairs.
                    Some(pair.as_basic_type_enum().into())
                } else if lookup.len() == 1 && lookup.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false) {
                    // Unresolved single-uppercase name: a generic parameter
                    // (erasure MVP, mirrors sema). Known types resolve above.
                    Some(self.context.i64_type().into())
                } else {
                    panic!("unknown struct {n} in llvm_ty_for_sema")
                }
            }
            crate::sema::Ty::Own(inner) => {
                // Every `own` slot is a pair (uniform with trait objects).
                Some(self.own_pair_type(inner).as_basic_type_enum().into())
            }
            crate::sema::Ty::Float => Some(self.context.f32_type().into()),
            crate::sema::Ty::Double => Some(self.context.f64_type().into()),
            crate::sema::Ty::Generic(n, args) => {
                let lookup = n.rsplit("::").next().unwrap_or(n);
                if let Some(st) = self.struct_types.get(lookup) { Some(st.as_basic_type_enum().into()) }
                else if let Some(et) = self.enum_types.get(lookup) { Some(et.as_basic_type_enum().into()) }
                else if lookup.len() == 1 && lookup.chars().next().unwrap().is_ascii_uppercase() {
                    if !args.is_empty() { return self.llvm_ty_for_sema(&args[0]); }
                    Some(self.context.i64_type().into())
                }
                else { Some(self.context.ptr_type(inkwell::AddressSpace::default()).into()) }
            }
            crate::sema::Ty::Tuple(tys) => {
                let tys_llvm: Vec<BasicTypeEnum> = tys.iter().filter_map(|t| self.llvm_ty_for_sema(t)).collect();
                Some(self.context.struct_type(&tys_llvm, false).into())
            }
            crate::sema::Ty::Any => Some(self.context.ptr_type(inkwell::AddressSpace::default()).into()),
            crate::sema::Ty::Function(_, _) => Some(self.context.ptr_type(inkwell::AddressSpace::default()).into()),
            crate::sema::Ty::Array(el) => {
                if let Some(inner) = self.llvm_ty_for_sema(el) {
                    match inner {
                        BasicTypeEnum::IntType(it) => Some(it.array_type(16).into()),
                        BasicTypeEnum::PointerType(pt) => Some(pt.array_type(16).into()),
                        BasicTypeEnum::FloatType(ft) => Some(ft.array_type(16).into()),
                        BasicTypeEnum::StructType(st) => Some(st.array_type(16).into()),
                        BasicTypeEnum::ArrayType(at) => Some(at.array_type(16).into()),
                        _ => Some(self.context.i64_type().array_type(16).into()),
                    }
                } else {
                    Some(self.context.i64_type().array_type(16).into())
                }
            }
            crate::sema::Ty::FixedArray { elem, size } => {
                let n = size.unwrap_or(16) as u32;
                if let Some(inner) = self.llvm_ty_for_sema(elem) {
                    match inner {
                        BasicTypeEnum::IntType(it) => Some(it.array_type(n).into()),
                        BasicTypeEnum::PointerType(pt) => Some(pt.array_type(n).into()),
                        BasicTypeEnum::FloatType(ft) => Some(ft.array_type(n).into()),
                        BasicTypeEnum::StructType(st) => Some(st.array_type(n).into()),
                        BasicTypeEnum::ArrayType(at) => Some(at.array_type(n).into()),
                        _ => Some(self.context.i64_type().array_type(n).into()),
                    }
                } else {
                    Some(self.context.i64_type().array_type(n).into())
                }
            }
            crate::sema::Ty::Vec(elem) => {
                // `Vec(Any)` (undetermined) uses i64 slots.
                let inner = match elem.as_ref() {
                    crate::sema::Ty::Any => self.context.i64_type().into(),
                    _ => self.llvm_ty_for_sema(elem).unwrap_or_else(|| self.context.i64_type().into()),
                };
                Some(self.vec_struct_ty(inner).into())
            }
            crate::sema::Ty::Map { key, value } => {
                let k = match key.as_ref() {
                    crate::sema::Ty::Any => self.context.i64_type().into(),
                    _ => self.llvm_ty_for_sema(key.as_ref()).unwrap_or_else(|| self.context.i64_type().into()),
                };
                let v = match value.as_ref() {
                    crate::sema::Ty::Any => self.context.ptr_type(inkwell::AddressSpace::default()).into(),
                    _ => self.llvm_ty_for_sema(value.as_ref()).unwrap_or_else(|| self.context.i64_type().into()),
                };
                Some(self.map_struct_ty(k, v).into())
            }
            crate::sema::Ty::Pointer(_) => Some(
                self.context
                    .ptr_type(inkwell::AddressSpace::default())
                    .into(),
            ),
            // `task<T>` (Async-6): opaque runtime handle pointer.
            crate::sema::Ty::Task(_) => Some(
                self.context
                    .ptr_type(inkwell::AddressSpace::default())
                    .into(),
            ),
            crate::sema::Ty::Optional(el) => {
                let inner = self.llvm_ty_for_sema(el).unwrap();
                Some(
                    self.context
                        .struct_type(
                            &[inner.into(), self.context.bool_type().into()],
                            false,
                        )
                        .into(),
                )
            }
            crate::sema::Ty::Enum(n) => {
                let lookup = n.rsplit("::").next().unwrap_or(n);
                let et = self.enum_types.get(lookup).unwrap_or_else(|| panic!("unknown enum {n} in llvm_ty_for_sema"));
                Some(et.as_basic_type_enum().into())
            }
        }
    }

    fn resolve_ty_for_codegen(&self, ty: &crate::sema::Ty) -> crate::sema::Ty {
        match ty {
            crate::sema::Ty::Struct(n) if self.enum_types.contains_key(n) => crate::sema::Ty::Enum(n.clone()),
            other => other.clone(),
        }
    }

    fn declare_function(&mut self, f: &Function) -> Result<(), CodegenError> {
        let ret_sema_raw: crate::sema::Ty = (&f.ret_ty).into();
        let ret_sema = self.resolve_ty_for_codegen(&ret_sema_raw);
        let param_semas: Vec<crate::sema::Ty> = f.params.iter().enumerate().map(|(idx, p)| {
            let raw: crate::sema::Ty = (&p.ty).into();
            let res = self.resolve_ty_for_codegen(&raw);
            if p.is_variadic {
                if p.ty.name() == "__derived__" {
                    if idx == 0 {
                        crate::sema::Ty::Array(Box::new(crate::sema::Ty::Int))
                    } else {
                        let prev_raw: crate::sema::Ty = (&f.params[idx-1].ty).into();
                        let prev_res = self.resolve_ty_for_codegen(&prev_raw);
                        crate::sema::Ty::Array(Box::new(prev_res))
                    }
                } else {
                    crate::sema::Ty::Array(Box::new(res))
                }
            } else {
                res
            }
        }).collect();
        let param_modes: Vec<ParamMode> = f.params.iter().map(|p| p.mode).collect();
        let param_is_variadic: Vec<bool> = f.params.iter().map(|p| p.is_variadic).collect();

        let param_types: Vec<inkwell::types::BasicMetadataTypeEnum> = f
            .params
            .iter()
            .filter(|p| !(p.is_variadic && p.name.is_empty())) // `...` alone for C varargs has no param, not a real param
            .map(|p| {
                if p.mode != ParamMode::None {
                    self.context.ptr_type(inkwell::AddressSpace::default()).into()
                } else if p.is_variadic {
                    // `...T vda` where `vda` is `T[]` array, or `... vda` derived
                    let elem_ty: crate::sema::Ty = if p.ty.name() == "__derived__" {
                        if let Some(prev_idx) = f.params.iter().position(|x| x.name == p.name) {
                            if prev_idx > 0 {
                                (&f.params[prev_idx-1].ty).into()
                            } else {
                                crate::sema::Ty::Int
                            }
                        } else {
                            crate::sema::Ty::Int
                        }
                    } else {
                        (&p.ty).into()
                    };
                    let elem_rt = self.resolve_ty_for_codegen(&elem_ty);
                    if let Some(bt) = self.llvm_ty_for_sema(&elem_rt) {
                        if elem_rt == crate::sema::Ty::Int {
                            self.context.i64_type().array_type(16).into()
                        } else {
                            let elem_llvm = self.llvm_ty_for_sema(&elem_rt).unwrap();
                            match elem_llvm {
                                inkwell::types::BasicTypeEnum::PointerType(pt) => pt.array_type(16).into(),
                                inkwell::types::BasicTypeEnum::IntType(it) => it.array_type(16).into(),
                                _ => elem_llvm.into(),
                            }
                        }
                    } else {
                        self.context.ptr_type(inkwell::AddressSpace::default()).into()
                    }
                } else {
                    let t: crate::sema::Ty = (&p.ty).into();
                    let rt = self.resolve_ty_for_codegen(&t);
                    self.llvm_ty_for_sema(&rt).map(|bt| bt.into()).unwrap()
                }
            })
            .collect();
        let is_c_varargs = f.params.iter().any(|p| p.is_variadic && p.name.is_empty());
        // Special ABI for `main`: every form is C `i32 (i32 argc, ptr argv)`
        // at LLVM level — even the no-args forms (crt0 always passes
        // argc/argv; undeclared extras would simply go unread). The prologue
        // captures argv[0] for `__hella_progname()`, and the `string[] args`
        // form additionally fills `args` from argv[1..] (see below).
        let is_main_with_args = f.name == "main"
            && f.params.len() == 1
            && f.params[0].name == "args"
            && matches!(&f.params[0].ty, Type::Array(el, _) if matches!(el.as_ref(), Type::String(_)));
        let fn_ty = if f.name == "main" && f.is_async {
            // Async-6 root executor: the user body becomes `__hella_async_main`
            // (sync LLVM signature); the public `main` wrapper spawns it and
            // joins, translating `void` -> exit 0 (see `codegen_async_main_wrapper`).
            let body_params: Vec<inkwell::types::BasicMetadataTypeEnum> = f
                .params
                .iter()
                .map(|p| {
                    let t: crate::sema::Ty = (&p.ty).into();
                    let rt = self.resolve_ty_for_codegen(&t);
                    self.llvm_ty_for_sema(&rt).map(|bt| bt.into()).unwrap()
                })
                .collect();
            let body_ty = if ret_sema == crate::sema::Ty::Void {
                self.context.void_type().fn_type(&body_params, false)
            } else {
                let rt = self.llvm_ty_for_sema(&ret_sema).unwrap_or(self.context.i64_type().into());
                rt.fn_type(&body_params, false)
            };
            let body_fn = self.module.add_function("__hella_async_main", body_ty, None);
            self.funcs.insert(
                "__hella_async_main".to_string(),
                (
                    body_fn,
                    TyInfo {
                        ret: ret_sema.clone(),
                        params: param_semas.clone(),
                        param_modes: param_modes.clone(),
                        param_names: f.params.iter().map(|p| p.name.clone()).collect(),
                        param_is_variadic: f.params.iter().map(|p| p.is_variadic).collect(),
                        param_defaults: f.params.iter().map(|p| p.default.clone()).collect(),
                        is_async: false, // the body runs inline inside the root task
                    },
                ),
            );
            // Wrapper `main`: C ABI, no params.
            let ptr = self.context.ptr_type(inkwell::AddressSpace::default());
            self.context.i32_type().fn_type(&[self.context.i32_type().into(), ptr.into()], false)
        } else if f.name == "main" {
            let ptr = self.context.ptr_type(inkwell::AddressSpace::default());
            self.context.i32_type().fn_type(&[self.context.i32_type().into(), ptr.into()], false)
        } else {
            match ret_sema {
                crate::sema::Ty::Void => {
                    self.context.void_type().fn_type(&param_types, is_c_varargs)
                }
                crate::sema::Ty::Int => {
                    self.context.i64_type().fn_type(&param_types, is_c_varargs)
                }
                crate::sema::Ty::UInt => {
                    self.context.i64_type().fn_type(&param_types, is_c_varargs)
                }
                crate::sema::Ty::SizedInt { bits, .. } => {
                    self.llvm_int_for_bits(bits).fn_type(&param_types, is_c_varargs)
                }
                crate::sema::Ty::Bool => {
                    self.context.bool_type().fn_type(&param_types, is_c_varargs)
                }
                crate::sema::Ty::Char => {
                    self.context.i32_type().fn_type(&param_types, is_c_varargs)
                }
                crate::sema::Ty::String => self
                    .context
                    .ptr_type(inkwell::AddressSpace::default())
                    .fn_type(&param_types, is_c_varargs),
                crate::sema::Ty::Struct(ref n) if n.len()==1 && n.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false) && {
                    let lookup = n.rsplit("::").next().unwrap_or(n);
                    !self.struct_types.contains_key(lookup) && !self.enum_types.contains_key(lookup) && self.trait_pair_of(lookup).is_none()
                } => {
                    // Unresolved single-uppercase return: a generic parameter
                    // (erasure MVP). Known types fall through below.
                    self.context.i64_type().fn_type(&param_types, is_c_varargs)
                }
                crate::sema::Ty::Own(ref inner) => {
                    self.own_pair_type(inner).fn_type(&param_types, is_c_varargs)
                }
                // `task<T>` (Async-6): opaque runtime handle pointer.
                crate::sema::Ty::Task(_) => self.context.ptr_type(inkwell::AddressSpace::default()).fn_type(&param_types, is_c_varargs),
                crate::sema::Ty::Struct(ref n) => {
                    if let Some(pair) = self.trait_pair_of(n) {
                        pair.fn_type(&param_types, is_c_varargs)
                    } else {
                        let st = self.struct_types.get(n).ok_or(CodegenError {
                            message: format!("unknown struct {n}"),
                            span: f.ret_ty.span(),
                        })?;
                        st.fn_type(&param_types, is_c_varargs)
                    }
                }
                crate::sema::Ty::Array(ref el) => {
                    // arrays as fixed [16 x elem] return — rarely used but support
                    let elem_ty = self.llvm_ty_for_sema(el).unwrap();
                    // For array element i64, array type is [16 x i64]
                    let arr_ty = self.context.i64_type().array_type(16);
                    arr_ty.fn_type(&param_types, is_c_varargs)
                }
                crate::sema::Ty::FixedArray { elem: ref elem, size: ref size } => {
                    let n = size.unwrap_or(16) as u32;
                    let elem_ty = self.llvm_ty_for_sema(elem.as_ref()).unwrap();
                    match elem_ty {
                        BasicTypeEnum::IntType(it) => it.array_type(n).fn_type(&param_types, is_c_varargs),
                        BasicTypeEnum::FloatType(ft) => ft.array_type(n).fn_type(&param_types, is_c_varargs),
                        BasicTypeEnum::PointerType(pt) => pt.array_type(n).fn_type(&param_types, is_c_varargs),
                        BasicTypeEnum::StructType(st) => st.array_type(n).fn_type(&param_types, is_c_varargs),
                        BasicTypeEnum::ArrayType(at) => at.array_type(n).fn_type(&param_types, is_c_varargs),
                        _ => self.context.i64_type().array_type(n).fn_type(&param_types, is_c_varargs),
                    }
                }
                crate::sema::Ty::Vec(ref elem) => {
                    let inner = match elem.as_ref() {
                        crate::sema::Ty::Any => self.context.i64_type().into(),
                        _ => self.llvm_ty_for_sema(elem).unwrap_or_else(|| self.context.i64_type().into()),
                    };
                    self.vec_struct_ty(inner).fn_type(&param_types, is_c_varargs)
                }
                crate::sema::Ty::Map { key: ref key, value: ref value } => {
                    let k = match key.as_ref() {
                        crate::sema::Ty::Any => self.context.i64_type().into(),
                        _ => self.llvm_ty_for_sema(key.as_ref()).unwrap_or_else(|| self.context.i64_type().into()),
                    };
                    let v = match value.as_ref() {
                        crate::sema::Ty::Any => self.context.ptr_type(inkwell::AddressSpace::default()).into(),
                        _ => self.llvm_ty_for_sema(value.as_ref()).unwrap_or_else(|| self.context.i64_type().into()),
                    };
                    self.map_struct_ty(k, v).fn_type(&param_types, is_c_varargs)
                }
                crate::sema::Ty::Pointer(_) => self
                    .context
                    .ptr_type(inkwell::AddressSpace::default())
                    .fn_type(&param_types, is_c_varargs),
                crate::sema::Ty::Optional(ref el) => {
                    let inner = self.llvm_ty_for_sema(el).unwrap();
                    self.context
                        .struct_type(
                            &[inner.into(), self.context.bool_type().into()],
                            false,
                        )
                        .fn_type(&param_types, is_c_varargs)
                }
                crate::sema::Ty::Enum(ref n) => {
                    let et = self.enum_types.get(n).ok_or(CodegenError{message: format!("unknown enum {n}"), span: f.ret_ty.span()})?;
                    et.fn_type(&param_types, is_c_varargs)
                }
                crate::sema::Ty::Float => self.context.f32_type().fn_type(&param_types, is_c_varargs),
                crate::sema::Ty::Double => self.context.f64_type().fn_type(&param_types, is_c_varargs),
                crate::sema::Ty::Generic(ref n, _) if n.len()==1 && n.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false) => self.context.i64_type().fn_type(&param_types, is_c_varargs),
                crate::sema::Ty::Generic(_, _) => self.context.ptr_type(inkwell::AddressSpace::default()).fn_type(&param_types, is_c_varargs),
                crate::sema::Ty::Tuple(ref tys) => self.tuple_struct_ty(tys).map(|st| st.fn_type(&param_types, is_c_varargs)).unwrap_or_else(|| self.context.ptr_type(inkwell::AddressSpace::default()).fn_type(&param_types, is_c_varargs)),
                crate::sema::Ty::Any => self.context.ptr_type(inkwell::AddressSpace::default()).fn_type(&param_types, is_c_varargs),
                crate::sema::Ty::Function(_, _) => self.context.ptr_type(inkwell::AddressSpace::default()).fn_type(&param_types, is_c_varargs),
            }
        };

        let func = self.module.add_function(&f.name, fn_ty, None);
        // Async-6: declare keeps the SYNC signature; the async-ness rides
        // along in TyInfo so call sites know to spawn instead of call.
        let is_async_fn = f.is_async;
        self.funcs.insert(
            f.name.clone(),
            (
                func,
                TyInfo {
                    ret: ret_sema,
                    params: param_semas,
                    param_modes,
                    param_names: f.params.iter().map(|p| p.name.clone()).collect(),
                    param_is_variadic: f.params.iter().map(|p| p.is_variadic).collect(),
                    param_defaults: f.params.iter().map(|p| p.default.clone()).collect(),
                    is_async: is_async_fn,
                },
            ),
        );
        Ok(())
    }

    fn get_or_declare_puts(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("puts") { return f; }
        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
        let fn_ty = self.context.i32_type().fn_type(&[ptr_ty.into()], false);
        self.module.add_function("puts", fn_ty, None)
    }
    fn get_or_declare_printf(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("printf") { return f; }
        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
        let fn_ty = self.context.i32_type().fn_type(&[ptr_ty.into()], true);
        self.module.add_function("printf", fn_ty, None)
    }
    fn get_or_declare_putchar(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("putchar") { return f; }
        let fn_ty = self.context.i32_type().fn_type(&[self.context.i32_type().into()], false);
        self.module.add_function("putchar", fn_ty, None)
    }
    fn get_or_declare_abort(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("abort") { return f; }
        let fn_ty = self.context.void_type().fn_type(&[], false);
        self.module.add_function("abort", fn_ty, None)
    }
    fn get_or_declare_strcpy(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("strcpy") { return f; }
        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
        let fn_ty = ptr_ty.fn_type(&[ptr_ty.into(), ptr_ty.into()], false);
        self.module.add_function("strcpy", fn_ty, None)
    }
    fn get_or_declare_strcat(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("strcat") { return f; }
        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
        let fn_ty = ptr_ty.fn_type(&[ptr_ty.into(), ptr_ty.into()], false);
        self.module.add_function("strcat", fn_ty, None)
    }
    fn get_or_declare_sprintf(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("sprintf") { return f; }
        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
        let fn_ty = self.context.i32_type().fn_type(&[ptr_ty.into(), ptr_ty.into()], true);
        self.module.add_function("sprintf", fn_ty, None)
    }
    fn get_or_declare_snprintf(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("snprintf") { return f; }
        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
        let fn_ty = self.context.i32_type().fn_type(&[ptr_ty.into(), self.context.i64_type().into(), ptr_ty.into()], true);
        self.module.add_function("snprintf", fn_ty, None)
    }
    fn get_or_declare_strncat(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("strncat") { return f; }
        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
        let fn_ty = ptr_ty.fn_type(&[ptr_ty.into(), ptr_ty.into(), self.context.i64_type().into()], false);
        self.module.add_function("strncat", fn_ty, None)
    }
    fn get_or_declare_strdup(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("strdup") { return f; }
        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
        let fn_ty = ptr_ty.fn_type(&[ptr_ty.into()], false);
        self.module.add_function("strdup", fn_ty, None)
    }
    fn get_or_declare_malloc(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("malloc") { return f; }
        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
        let fn_ty = ptr_ty.fn_type(&[self.context.i64_type().into()], false);
        self.module.add_function("malloc", fn_ty, None)
    }
    fn get_or_declare_free(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("free") { return f; }
        let fn_ty = self.context.void_type().fn_type(&[self.context.ptr_type(inkwell::AddressSpace::default()).into()], false);
        self.module.add_function("free", fn_ty, None)
    }
    fn get_or_declare_memcpy(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("memcpy") { return f; }
        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
        let fn_ty = ptr_ty.fn_type(&[ptr_ty.into(), ptr_ty.into(), self.context.i64_type().into()], false);
        self.module.add_function("memcpy", fn_ty, None)
    }
    /// `hella_task_spawn(entry, arg, result_size)` (Async-7): allocate and
    /// launch a task. `entry` is `void*(task_handle, arg)`. Declared lazily
    /// so sync programs never reference the runtime (Async-8).
    fn get_or_declare_task_spawn(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("hella_task_spawn") { return f; }
        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
        let i64_ty = self.context.i64_type();
        let entry_ty = ptr_ty.fn_type(&[ptr_ty.into(), ptr_ty.into()], false);
        let fn_ty = ptr_ty.fn_type(&[entry_ty.ptr_type(inkwell::AddressSpace::default()).into(), ptr_ty.into(), i64_ty.into()], false);
        self.module.add_function("hella_task_spawn", fn_ty, None)
    }
    /// `hella_task_join(handle)` (Async-7): block until completion, consume
    /// exactly once. Lazy for the same reason.
    fn get_or_declare_task_join(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("hella_task_join") { return f; }
        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
        let fn_ty = self.context.i32_type().fn_type(&[ptr_ty.into()], false);
        self.module.add_function("hella_task_join", fn_ty, None)
    }
    /// `hella_task_result(handle, dst, n)` (Async-7): copy out the result
    /// and free the task. Lazy for the same reason.
    fn get_or_declare_task_result(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("hella_task_result") { return f; }
        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
        let fn_ty = self.context.void_type().fn_type(&[ptr_ty.into(), ptr_ty.into(), self.context.i64_type().into()], false);
        self.module.add_function("hella_task_result", fn_ty, None)
    }
    /// `hella_task_store_inline(handle, src, n)` (Async-7): worker-side
    /// store of a small (<=16B) result into the task. Lazy as above.
    fn get_or_declare_task_store_inline(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("hella_task_store_inline") { return f; }
        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
        let fn_ty = self.context.void_type().fn_type(&[ptr_ty.into(), ptr_ty.into(), self.context.i64_type().into()], false);
        self.module.add_function("hella_task_store_inline", fn_ty, None)
    }
    /// `hella_task_store_spill(handle, src, n)` (Async-7): worker-side
    /// store of a large result into the task's heap buffer. Lazy as above.
    fn get_or_declare_task_store_spill(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("hella_task_store_spill") { return f; }
        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
        let fn_ty = self.context.void_type().fn_type(&[ptr_ty.into(), ptr_ty.into(), self.context.i64_type().into()], false);
        self.module.add_function("hella_task_store_spill", fn_ty, None)
    }
    /// `sched_yield()` (Async-6/Async-7): cooperative checkpoint backing
    /// `yield`. Declared lazily — only async programs reference it, so sync
    /// binaries never gain the dependency (Async-8). POSIX provides it in
    /// libc; on Windows the CLI links a small shim (see `hella_rt.c`).
    fn get_or_declare_sched_yield(&self) -> FunctionValue<'ctx> {
        if let Some(f) = self.module.get_function("sched_yield") { return f; }
        let fn_ty = self.context.i32_type().fn_type(&[], false);
        self.module.add_function("sched_yield", fn_ty, None)
    }

    // -- Async lowering (Async-6/Async-7) -----------------------------------

    /// Byte size of an LLVM type for the task result transport. Exact for
    /// the scalar/struct shapes async functions return (raw-byte copy).
    fn llvm_byte_size(&self, ty: BasicTypeEnum<'ctx>) -> u64 {
        match ty {
            BasicTypeEnum::IntType(it) => ((it.get_bit_width() as u64 + 7) / 8).max(1),
            BasicTypeEnum::FloatType(ft) => if ft == self.context.f32_type() { 4 } else { 8 },
            BasicTypeEnum::PointerType(_) => 8,
            BasicTypeEnum::StructType(st) => {
                let mut total = 0u64;
                for i in 0..st.count_fields() {
                    let fs = self.llvm_byte_size(st.get_field_type_at_index(i).unwrap());
                    total += fs.max(8);
                }
                total
            }
            BasicTypeEnum::ArrayType(at) =>
                at.len() as u64 * self.llvm_byte_size(at.get_element_type().into()),
            BasicTypeEnum::VectorType(vt) =>
                vt.get_size() as u64 * self.llvm_byte_size(vt.get_element_type().into()),
            BasicTypeEnum::ScalableVectorType(vt) =>
                vt.get_size() as u64 * self.llvm_byte_size(vt.get_element_type().into()),
        }
    }

    /// Root executor for `async main` (Async-6): `main` (C ABI, no visible
    /// params beyond the standard argc/argv pair) spawns
    /// `__hella_async_main(args...)` as the root task and joins it,
    /// returning the exit code (`int` main) or 0 (`void` main).
    fn codegen_async_main_wrapper(
        &mut self,
        body_fn: FunctionValue<'ctx>,
        f: &Function,
    ) -> Result<(), CodegenError> {
        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
        let i32_ty = self.context.i32_type();
        let i64_ty = self.context.i64_type();
        let main_fn = self.funcs.get("main").map(|(fv, _)| *fv).unwrap();
        let entry = self.context.append_basic_block(main_fn, "entry");
        self.cur_fn = Some(main_fn);
        self.cur_is_main = true;
        self.builder.position_at_end(entry);
        self.vars.push(HashMap::new());
        self.own_slots.push(Vec::new());
        self.scope_dtors.push(Vec::new());

        // Capture argv[0] (mirrors the normal main prologue).
        {
            let argc = self.builder.build_int_s_extend(
                main_fn.get_nth_param(0).unwrap().into_int_value(), i64_ty, "argc64").unwrap();
            let argv = main_fn.get_nth_param(1).unwrap().into_pointer_value();
            let has = self.builder.build_int_compare(IntPredicate::SGT, argc, i64_ty.const_zero(), "argv0.has").unwrap();
            let slot0 = unsafe { self.builder.build_gep(ptr_ty, argv, &[i64_ty.const_zero()], "argv0.slot").unwrap() };
            let s0 = self.builder.build_load(ptr_ty, slot0, "argv0.str").unwrap();
            let v0 = self.builder.build_select(has, s0, ptr_ty.const_null().into(), "argv0").unwrap();
            self.builder.build_store(self.argv0_global(), v0).unwrap();
        }

        // Build body args: `int main(string[] args) async` fills args from
        // argv[1..] exactly like the sync form.
        let body_info = self.funcs.get("__hella_async_main").unwrap().1.clone();
        let mut body_args: Vec<inkwell::values::BasicMetadataValueEnum> = Vec::new();
        if f.params.len() == 1 && f.params[0].name == "args" {
            // [16 x ptr] from argv[1..]
            let argc = main_fn.get_nth_param(0).unwrap().into_int_value();
            let argv = main_fn.get_nth_param(1).unwrap().into_pointer_value();
            let arr_ty = ptr_ty.array_type(16);
            let arr_alloca = self.builder.build_alloca(arr_ty, "async.main.args").unwrap();
            let one = i32_ty.const_int(1, false);
            let cap = i32_ty.const_int(16, false);
            for i in 0..16u32 {
                let i_c = i32_ty.const_int(i as u64 + 1, false); // argv[1..]
                let in_range = self.builder.build_int_compare(IntPredicate::SLT, i_c, argc, "arg.in").unwrap();
                let slot = unsafe { self.builder.build_gep(ptr_ty, argv, &[i64_ty.const_int(i as u64 + 1, false)], "arg.slot").unwrap() };
                let loaded = self.builder.build_load(ptr_ty, slot, "arg.v").unwrap();
                let sel = self.builder.build_select(in_range, loaded, ptr_ty.const_null().into(), "arg.sel").unwrap();
                let dst = unsafe { self.builder.build_gep(arr_ty, arr_alloca, &[i64_ty.const_zero(), i64_ty.const_int(i as u64, false)], "arg.dst").unwrap() };
                self.builder.build_store(dst, sel).unwrap();
            }
            body_args.push(self.builder.build_load(arr_ty, arr_alloca, "args.load").unwrap().into());
        }
        let _ = body_info;

        // ret_size: 0 for void, else size of the LLVM ret type.
        let ret_sema: crate::sema::Ty = (&f.ret_ty).into();
        let ret_sema = self.resolve_ty_for_codegen(&ret_sema);
        let (ret_size, ret_llvm) = if ret_sema == crate::sema::Ty::Void {
            (0u64, None)
        } else {
            let rt = self.llvm_ty_for_sema(&ret_sema).unwrap_or(i64_ty.into());
            (self.llvm_byte_size(rt), Some(rt))
        };

        // task = spawn(entry=__async_entry___hella_async_main, ctx, ret_size)
        // Reuse the generic spawn-call lowering by faking a call: build the
        // ctx from body_args. (Simpler than a bespoke path: the generic
        // path generates the trampoline + heap ctx.)
        let handle = self.codegen_async_spawn_call(body_fn, &TyInfo {
            ret: ret_sema.clone(),
            params: vec![],
            param_modes: vec![],
            param_names: vec![],
            param_is_variadic: vec![],
            param_defaults: vec![],
            is_async: true,
        }, body_args, f.span)?;
        let handle_ptr = handle.into_pointer_value();

        // join + result
        let join = self.get_or_declare_task_join();
        self.builder.build_call(join, &[handle_ptr.into()], "async.main.join").unwrap();
        let res_fn = self.get_or_declare_task_result();
        let exit: inkwell::values::IntValue = if let Some(rt) = ret_llvm {
            let slot = self.builder.build_alloca(rt, "async.main.ret").unwrap();
            self.builder.build_call(res_fn, &[handle_ptr.into(), slot.into(), i64_ty.const_int(ret_size, false).into()], "async.main.result").unwrap();
            let v = self.builder.build_load(rt, slot, "async.main.ret.load").unwrap().into_int_value();
            self.builder.build_int_truncate(v, i32_ty, "exit32").unwrap()
        } else {
            let scratch = self.builder.build_alloca(i64_ty, "async.main.scratch").unwrap();
            self.builder.build_call(res_fn, &[handle_ptr.into(), scratch.into(), i64_ty.const_int(0, false).into()], "async.main.result").unwrap();
            i32_ty.const_int(0, false)
        };
        self.emit_global_dtors();
        self.builder.build_return(Some(&exit)).unwrap();

        self.own_slots.pop();
        self.scope_dtors.pop();
        self.vars.pop();
        self.cur_fn = None;
        self.cur_is_main = false;
        if !main_fn.verify(true) {
            return Err(CodegenError {
                message: "async main wrapper failed verification".into(),
                span: f.span,
            });
        }
        Ok(())
    }

    /// Spawn `func(args)` as a task (Async-6). The async callee keeps its
    /// SYNC LLVM signature (`params -> Ret`); we box the packed args into a
    /// per-call heap ctx, generate a static trampoline
    /// `__async_entry_<callee>` that unpacks, calls the body, stores the
    /// result into the task (inline <=16B, spill otherwise), frees the ctx,
    /// and returns null. Then `hella_task_spawn(entry, ctx, ret_size)`
    /// yields the `task<Ret>` handle.
    fn codegen_async_spawn_call(
        &mut self,
        func: FunctionValue<'ctx>,
        info: &TyInfo,
        arg_vals: Vec<inkwell::values::BasicMetadataValueEnum<'ctx>>,
        span: Span,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        use inkwell::values::BasicMetadataValueEnum as M;
        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
        let i64_ty = self.context.i64_type();
        let callee = func.get_name().to_str().unwrap_or("<async>").to_string();

        // Result transport size (0 for void).
        let ret_llvm: Option<BasicTypeEnum<'ctx>> = match &info.ret {
            crate::sema::Ty::Void => None,
            t => Some(self.llvm_ty_for_sema(t).ok_or(CodegenError {
                message: format!("async function `{callee}` returns a type with no lowering: {t}"),
                span,
            })?),
        };
        let ret_size: u64 = ret_llvm.map(|t| self.llvm_byte_size(t)).unwrap_or(0);

        // Context struct: one field per packed arg.
        let ctx_fields: Vec<BasicTypeEnum<'ctx>> = arg_vals.iter().map(|a| match a {
            M::IntValue(v) => v.get_type().into(),
            M::FloatValue(v) => v.get_type().into(),
            M::PointerValue(v) => v.get_type().into(),
            M::ArrayValue(v) => v.get_type().into(),
            M::StructValue(v) => v.get_type().into(),
            M::VectorValue(v) => v.get_type().into(),
            M::ScalableVectorValue(v) => v.get_type().into(),
            M::MetadataValue(_) => i64_ty.into(),
        }).collect();
        let ctx_ty = self.context.struct_type(&ctx_fields, false);

        // Heap-allocate + fill the ctx (freed by the trampoline).
        let malloc = self.get_or_declare_malloc();
        let ctx_size = self.llvm_byte_size(ctx_ty.into()).max(1);
        let ctx_ptr = self.builder
            .build_call(malloc, &[i64_ty.const_int(ctx_size, false).into()], "async.ctx.malloc")
            .unwrap().try_as_basic_value().basic().unwrap().into_pointer_value();
        for (i, a) in arg_vals.iter().enumerate() {
            let val: BasicValueEnum = match a {
                M::IntValue(v) => (*v).into(),
                M::FloatValue(v) => (*v).into(),
                M::PointerValue(v) => (*v).into(),
                M::ArrayValue(v) => (*v).into(),
                M::StructValue(v) => (*v).into(),
                M::VectorValue(v) => (*v).into(),
                M::ScalableVectorValue(v) => (*v).into(),
                M::MetadataValue(_) => continue,
            };
            let gep = self.builder.build_struct_gep(ctx_ty, ctx_ptr, i as u32, "async.ctx.field").unwrap();
            self.builder.build_store(gep, val).unwrap();
        }

        // Generate (or reuse) the static trampoline.
        let entry_name = format!("__async_entry_{callee}");
        let entry_fn = if let Some(f) = self.module.get_function(&entry_name) {
            f
        } else {
            let entry_ty = ptr_ty.fn_type(&[ptr_ty.into(), ptr_ty.into()], false);
            let ef = self.module.add_function(&entry_name, entry_ty, None);
            let entry_bb = self.context.append_basic_block(ef, "entry");
            let saved_pos = self.builder.get_insert_block();
            self.builder.position_at_end(entry_bb);
            let task_h = ef.get_nth_param(0).unwrap().into_pointer_value();
            let ctx_in = ef.get_nth_param(1).unwrap().into_pointer_value();
            let mut call_args: Vec<M> = Vec::with_capacity(ctx_fields.len());
            for (i, ft) in ctx_fields.iter().enumerate() {
                let gep = self.builder.build_struct_gep(ctx_ty, ctx_in, i as u32, "async.arg").unwrap();
                let lv = self.builder.build_load(*ft, gep, "async.arg.load").unwrap();
                call_args.push(lv.into());
            }
            let call = self.builder.build_call(func, &call_args, "async.body").unwrap();
            if let Some(rt) = ret_llvm {
                let val = call.try_as_basic_value().basic().ok_or(CodegenError {
                    message: format!("async body of `{callee}` returned void unexpectedly"),
                    span,
                })?;
                let slot = self.builder.build_alloca(rt, "async.ret.slot").unwrap();
                self.builder.build_store(slot, val).unwrap();
                let n = i64_ty.const_int(ret_size, false);
                let store_fn = if ret_size <= 16 {
                    self.get_or_declare_task_store_inline()
                } else {
                    self.get_or_declare_task_store_spill()
                };
                self.builder.build_call(store_fn, &[task_h.into(), slot.into(), n.into()], "async.store").unwrap();
            }
            let free = self.get_or_declare_free();
            self.builder.build_call(free, &[ctx_in.into()], "async.ctx.free").unwrap();
            self.builder.build_return(Some(&ptr_ty.const_null())).unwrap();
            if let Some(bb) = saved_pos { self.builder.position_at_end(bb); }
            if !ef.verify(true) {
                return Err(CodegenError {
                    message: format!("async trampoline for `{callee}` failed verification"),
                    span,
                });
            }
            ef
        };

        let spawn = self.get_or_declare_task_spawn();
        let handle = self.builder.build_call(spawn, &[
            entry_fn.as_global_value().as_pointer_value().into(),
            ctx_ptr.into(),
            i64_ty.const_int(ret_size, false).into(),
        ], "async.spawn").unwrap().try_as_basic_value().basic().unwrap();
        Ok(handle)
    }

    // NOTE (real stdlib): no `is_stdlib_io_intrinsic` /
    // `codegen_stdlib_io_body`. User-facing IO (`print`, `println`,
    // `printInt`, `putChar`) is ordinary Hella in `stdlib/std/io.hll` on top
    // of `extern` libc declarations. The `get_or_declare_*` helpers below
    // remain for compiler-internal lowering only (assert, interpolation).

    fn codegen_call_arg(&mut self, arg: &CallArg) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        match arg {
            CallArg::Expr(e) => self.codegen_expr(e),
            CallArg::Named { value, .. } => self.codegen_expr(value),
            CallArg::Out { name, name_span, .. } => {
                let (ptr, _) = self.lookup_var(name).ok_or(CodegenError { message: format!("undefined variable `{}` for `out`", name), span: *name_span })?;
                Ok(ptr.into())
            }
            CallArg::Ref { expr, .. } => {
                let ptr = self.codegen_as_ptr(expr)?;
                Ok(ptr.into())
            }
        }
    }

    /// Materialize an `out` call argument with no visible variable: alloca
    /// a slot (explicit annotation wins, else the declared parameter type,
    /// else an opaque `any` slot), register it like a `VarDecl` (including
    /// vec/map/string tracking and destructors), and return its pointer.
    /// Existing variables (and non-`out` args) yield `None` — callers fall
    /// through to normal resolution.
    fn materialize_out_var(
        &mut self,
        arg: &CallArg,
        param_sema_ty: Option<&crate::sema::Ty>,
    ) -> Result<Option<PointerValue<'ctx>>, CodegenError> {
        let CallArg::Out { name, ty: opt_ty, name_span, .. } = arg else {
            return Ok(None);
        };
        if self.lookup_var(name).is_some() {
            return Ok(None);
        }
        let llvm_ty = match opt_ty {
            Some(t) => self.llvm_ty_for(t),
            None => match param_sema_ty {
                Some(pt) => self.llvm_ty_for_sema(pt).unwrap_or_else(|| {
                    self.context.ptr_type(inkwell::AddressSpace::default()).into()
                }),
                // No signature context (unknown callee): opaque slot,
                // mirroring sema's `any` fallback.
                None => self.context.ptr_type(inkwell::AddressSpace::default()).into(),
            },
        };
        let alloca = self.create_entry_block_alloca(name, llvm_ty);
        self.vars.last_mut().ok_or(CodegenError {
            message: "outside function".into(),
            span: *name_span,
        })?.insert(name.clone(), (alloca, llvm_ty));
        // Track like VarDecl so later uses lower correctly.
        let is_vec = opt_ty.as_ref().is_some_and(|t| matches!(t, Type::Vec { .. }))
            || matches!(param_sema_ty, Some(crate::sema::Ty::Vec(_)));
        let is_map = opt_ty.as_ref().is_some_and(|t| matches!(t, Type::Map { .. }))
            || matches!(param_sema_ty, Some(crate::sema::Ty::Map { .. }));
        let is_string = opt_ty.as_ref().is_some_and(|t| matches!(t, Type::String(_)))
            || matches!(param_sema_ty, Some(crate::sema::Ty::String));
        if is_vec {
            self.vec_vars.insert(name.clone());
        }
        if is_map {
            self.map_vars.insert(name.clone());
        }
        if is_string {
            self.string_vars.insert(name.clone());
        }
        if opt_ty.as_ref().is_some_and(Self::ast_ty_is_unsigned)
            || param_sema_ty.as_ref().is_some_and(|t| Self::sema_ty_is_unsigned(t))
        {
            self.unsigned_vars.insert(name.clone());
        }
        if let Some(class_name) = match opt_ty {
            Some(t) => self.dtor_name_for_ast_ty(t),
            None => None,
        }
        .or_else(|| match param_sema_ty {
            Some(crate::sema::Ty::Struct(n)) => {
                if self.class_destructors.contains_key(n) {
                    Some(n.clone())
                } else if self.struct_needs_field_destroy(n) {
                    Some(n.clone())
                } else {
                    None
                }
            }
            _ => None,
        }) {
            if let Some(top) = self.scope_dtors.last_mut() {
                top.push((alloca, class_name));
            }
        }
        // Fixed arrays of destructible elements register per index.
        // (`out` declarations carry an explicit AST type when present.)
        if let Some(t) = opt_ty {
            self.track_dtor_array_elems(alloca, t, llvm_ty);
        }
        Ok(Some(alloca))
    }

    /// Per-element scope-exit entries for a fixed-array slot whose element
    /// type needs destruction (user dtors or structural). No-op otherwise.
    fn track_dtor_array_elems(
        &mut self,
        alloca: PointerValue<'ctx>,
        ast_ty: &Type,
        llvm_ty: BasicTypeEnum<'ctx>,
    ) {
        let (elem_ast, arr_ty) = match (ast_ty, llvm_ty) {
            (Type::FixedArray { elem, .. }, BasicTypeEnum::ArrayType(at)) => (elem.as_ref(), at),
            (Type::Array(elem, _), BasicTypeEnum::ArrayType(at)) => (elem.as_ref(), at),
            _ => return,
        };
        let ename = match elem_ast {
            Type::Named(n, _) | Type::Generic(n, _, _) => n.rsplit("::").next().unwrap_or(n).to_string(),
            _ => return,
        };
        // User dtors and structural destruction share the entry shape.
        let entry = if self.class_destructors.contains_key(&ename) {
            Some(ename.clone())
        } else if self.struct_needs_field_destroy(&ename) {
            Some(ename.clone())
        } else {
            None
        };
        if let Some(entry) = entry {
            let ctx: &'ctx Context = self.context;
            let i64_ty = ctx.i64_type();
            for i in 0..arr_ty.len() {
                let eptr = unsafe {
                    self.builder
                        .build_gep(arr_ty, alloca, &[i64_ty.const_zero(), i64_ty.const_int(i as u64, false)], "arr.elem.dtor")
                        .unwrap()
                };
                if let Some(top) = self.scope_dtors.last_mut() {
                    top.push((eptr, entry.clone()));
                }
            }
        }
    }

    /// Lower one call argument against its declared parameter.
    /// - `ref` params take pointers: explicit `ref e` passes through
    ///   (already a pointer); plain lvalues address-take; anything else
    ///   is a compile error (sema rejects non-lvalues first).
    /// - `out` params take pointers: explicit `out x` passes through
    ///   without coercion; anything else was rejected in sema.
    /// - Otherwise evaluates to a value (trait-boxed, int-coerced).
    /// `param_idx` indexes `info` (including any leading `this`).
    fn codegen_arg_for_param(
        &mut self,
        arg: &CallArg,
        info: &TyInfo,
        param_idx: usize,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        // Implicit `out` declarations materialize their slot first so the
        // pointer resolution below finds them.
        self.materialize_out_var(arg, info.params.get(param_idx))?;
        match info.param_modes.get(param_idx) {
            Some(ParamMode::Ref) => match arg {
                CallArg::Ref { .. } => self.codegen_call_arg(arg),
                CallArg::Expr(e) | CallArg::Named { value: e, .. } => {
                    Ok(self.codegen_as_ptr(e)?.into())
                }
                CallArg::Out { name_span, .. } => Err(CodegenError {
                    message: "`out` argument passed to `ref` parameter".into(),
                    span: *name_span,
                }),
            },
            Some(ParamMode::Out) => {
                // Pointer already; must skip int coercion.
                self.codegen_call_arg(arg)
            }
            _ => {
                let v = self.codegen_call_arg(arg)?;
                // Move from owned source: null the source slot(s) so their
                // scope destroy becomes a no-op (sema poisoned them).
                // Transparent through `?:`/parens/match arms. Fires for
                // `own` params and for params with transitive `own` fields.
                let param_owned = match info.params.get(param_idx) {
                    Some(crate::sema::Ty::Own(_)) => true,
                    Some(crate::sema::Ty::Struct(n)) => self.struct_needs_field_destroy(n),
                    _ => false,
                };
                if param_owned {
                    let src_expr: Option<&Expr> = match arg {
                        CallArg::Expr(e) => Some(e),
                        CallArg::Named { value, .. } => Some(value),
                        _ => None,
                    };
                    if let Some(e) = src_expr {
                        self.null_moved_sources(e, None);
                    }
                }
                let v = self.box_arg_for_param(v, info, param_idx, arg.span())?;
                if let Some(pt) = info.params.get(param_idx) {
                    if let Some(dest) = self.llvm_ty_for_sema(pt) {
                        return Ok(self.coerce_to_ty(v, dest));
                    }
                }
                Ok(v)
            }
        }
    }

    /// Evaluate the default value for parameter `idx` (call-site fill for
    /// omitted trailing/named arguments; sema guarantees one exists) and
    /// lower it exactly like a provided argument.
    fn codegen_default_for_param(
        &mut self,
        info: &TyInfo,
        idx: usize,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let def = info
            .param_defaults
            .get(idx)
            .and_then(|d| d.clone())
            .ok_or(CodegenError {
                message: "missing argument with no default value".into(),
                span: Span::new(0, 0),
            })?;
        let arg = CallArg::Expr(def);
        self.codegen_arg_for_param(&arg, info, idx)
    }

    /// Pack a non-variadic call against `info`: positional arguments fill
    /// parameters in order, `named` arguments fill by parameter name
    /// (overriding positionals on conflict), and anything still missing is
    /// filled from defaults (sema-checked). `base` is the TyInfo index of
    /// the first user argument (0 for free functions, 1 for `this`-leading
    /// infos). Unfillable slots fall back to `i64` zero (unreachable after
    /// sema; preserves the legacy named-call shape).
    fn pack_call_args(
        &mut self,
        args: &[CallArg],
        info: &TyInfo,
        base: usize,
    ) -> Result<Vec<inkwell::values::BasicMetadataValueEnum<'ctx>>, CodegenError> {
        let mut out: Vec<inkwell::values::BasicMetadataValueEnum<'ctx>> = Vec::new();
        let mut named: std::collections::HashMap<String, &CallArg> = std::collections::HashMap::new();
        for a in args {
            if let CallArg::Named { name, .. } = a {
                named.insert(name.clone(), a);
            }
        }
        let positionals: Vec<&CallArg> = args
            .iter()
            .filter(|a| !matches!(a, CallArg::Named { .. }))
            .collect();
        let mut pos = 0;
        for (idx, pname) in info.param_names.iter().enumerate().skip(base) {
            if idx >= info.params.len() {
                break;
            }
            if let Some(arg) = named.get(pname) {
                out.push(self.codegen_arg_for_param(arg, info, idx)?.into());
            } else if pos < positionals.len() {
                out.push(self.codegen_arg_for_param(positionals[pos], info, idx)?.into());
                pos += 1;
            } else if let Ok(v) = self.codegen_default_for_param(info, idx) {
                out.push(v.into());
            } else {
                out.push(self.context.i64_type().const_int(0, false).into());
            }
        }
        // Extra positionals beyond the parameter list (sema already
        // diagnosed arity) still evaluate for side effects.
        while pos < positionals.len() {
            let _ = self.codegen_call_arg(positionals[pos])?;
            pos += 1;
        }
        Ok(out)
    }

    /// Clamped slice bounds `(lo, len)` with `0 <= lo` and `len >= 0`,
    /// relative to `full_len` (array size, vector length, or `strlen`).
    /// `end` defaults to the full length; `..=` includes it. Bound
    /// expressions evaluate once, in order.
    fn slice_bounds(
        &mut self,
        start: &Option<Box<Expr>>,
        end: &Option<Box<Expr>>,
        inclusive: bool,
        full_len: inkwell::values::IntValue<'ctx>,
    ) -> Result<(inkwell::values::IntValue<'ctx>, inkwell::values::IntValue<'ctx>), CodegenError> {
        // Copy the context reference out so the i64 type below does not
        // hold an immutable borrow of `self` across `&mut` calls.
        let ctx: &'ctx Context = self.context;
        let i64_ty = ctx.i64_type();
        let i64_as_basic: BasicTypeEnum<'ctx> = i64_ty.into();
        let lo_raw = if let Some(s) = start {
            let sv = self.codegen_expr(s)?;
            self.coerce_to_ty(sv, i64_as_basic).into_int_value()
        } else {
            i64_ty.const_zero()
        };
        let hi_raw = if let Some(e) = end {
            let ev0 = self.codegen_expr(e)?;
            let ev = self.coerce_to_ty(ev0, i64_as_basic).into_int_value();
            if inclusive { self.builder.build_int_add(ev, i64_ty.const_int(1, false), "slice.hi.incl").unwrap() } else { ev }
        } else {
            full_len
        };
        let lo_nonneg = self.builder.build_select(self.builder.build_int_compare(IntPredicate::SGT, lo_raw, i64_ty.const_zero(), "slice.lo.pos").unwrap(), lo_raw, i64_ty.const_zero(), "slice.lo.nn").unwrap().into_int_value();
        let lo = self.builder.build_select(self.builder.build_int_compare(IntPredicate::SGT, lo_nonneg, full_len, "slice.lo.over").unwrap(), full_len, lo_nonneg, "slice.lo").unwrap().into_int_value();
        let hi_nonneg = self.builder.build_select(self.builder.build_int_compare(IntPredicate::SGT, hi_raw, i64_ty.const_zero(), "slice.hi.pos").unwrap(), hi_raw, i64_ty.const_zero(), "slice.hi.nn").unwrap().into_int_value();
        let hi = self.builder.build_select(self.builder.build_int_compare(IntPredicate::SGT, hi_nonneg, full_len, "slice.hi.over").unwrap(), full_len, hi_nonneg, "slice.hi").unwrap().into_int_value();
        let raw_len = self.builder.build_int_sub(hi, lo, "slice.len.raw").unwrap();
        let len = self.builder.build_select(self.builder.build_int_compare(IntPredicate::SGT, raw_len, i64_ty.const_zero(), "slice.len.pos").unwrap(), raw_len, i64_ty.const_zero(), "slice.len").unwrap().into_int_value();
        Ok((lo, len))
    }

    /// Zero fill value for a slice element type.
    fn slice_zero_elem(
        &self,
        elem_ty: BasicTypeEnum<'ctx>,
        span: Span,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        match elem_ty {
            BasicTypeEnum::IntType(it) => Ok(it.const_zero().into()),
            BasicTypeEnum::PointerType(pt) => Ok(pt.const_null().into()),
            BasicTypeEnum::FloatType(ft) => Ok(ft.const_float(0.0).into()),
            BasicTypeEnum::StructType(st) => Ok(st.const_zero().into()),
            BasicTypeEnum::ArrayType(at) => Ok(at.const_zero().into()),
            _ => Err(CodegenError{message: "unsupported element type for slicing".into(), span}),
        }
    }

    /// Box a call argument into its declared parameter type when that
    /// type is a trait object; pass through otherwise.
    fn box_arg_for_param(
        &self,
        val: BasicValueEnum<'ctx>,
        info: &TyInfo,
        param_idx: usize,
        span: Span,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let Some(pt) = info.params.get(param_idx) else {
            return Ok(val);
        };
        let Some(dest) = self.llvm_ty_for_sema(pt) else {
            return Ok(val);
        };
        self.box_trait_value(val, dest, span)
    }

    /// Pack method-call arguments: `this` first, then fixed/variadic/tail
    /// params (with `this` offset), boxing trait arguments. Shared by
    /// static and trait dispatch.
    fn codegen_method_call_args(
        &mut self,
        args: &[CallArg],
        info: &TyInfo,
        this_ptr: PointerValue<'ctx>,
        span: Span,
    ) -> Result<Vec<inkwell::values::BasicMetadataValueEnum<'ctx>>, CodegenError> {
        let mut arg_vals: Vec<inkwell::values::BasicMetadataValueEnum<'ctx>> =
            vec![this_ptr.into()];
        // Variadic handling for method `...T vda` (with `this` offset)
        let variadic_idx = info.param_is_variadic.iter().position(|&v| v);
        if let Some(vidx) = variadic_idx {
            // vidx includes `this` at 0, so real fixed before variadic = vidx -1
            let fixed_real = if vidx == 0 { 0 } else { vidx - 1 };
            // push fixed real params
            for (i, a) in args.iter().take(fixed_real).enumerate() {
                let v = self.codegen_arg_for_param(a, info, i + 1)?;
                arg_vals.push(v.into());
            }
            // variadic element type
            let elem_ty = info.params.get(vidx).and_then(|t| if let crate::sema::Ty::Array(el) = t { Some(&**el) } else { None }).cloned().unwrap_or(crate::sema::Ty::Int);
            let arr_llvm_ty: BasicTypeEnum = if let Some(bt) = self.llvm_ty_for_sema(&elem_ty) {
                match bt {
                    BasicTypeEnum::PointerType(pt) => pt.array_type(16).into(),
                    BasicTypeEnum::IntType(it) => it.array_type(16).into(),
                    BasicTypeEnum::FloatType(ft) => ft.array_type(16).into(),
                    BasicTypeEnum::StructType(st) => st.array_type(16).into(),
                    BasicTypeEnum::ArrayType(at) => at.array_type(16).into(),
                    _ => self.context.i64_type().array_type(16).into(),
                }
            } else {
                self.context.i64_type().array_type(16).into()
            };
            let total_real_params = info.params.len() - 1; // excluding this
            let remaining_after = total_real_params - (vidx - 1) - 1; // params after variadic
            let vda_count = if remaining_after == 0 {
                args.len() - fixed_real
            } else {
                if args.len() >= total_real_params { args.len() - total_real_params + 1 } else { 0 }
            };
            let elem_dest: Option<BasicTypeEnum> = self.llvm_ty_for_sema(&elem_ty);
            let mut arr_val: BasicValueEnum = arr_llvm_ty.into_array_type().get_undef().into();
            if args.len() <= fixed_real {
                arr_val = arr_llvm_ty.const_zero().into();
            } else {
                for (j, arg) in args.iter().skip(fixed_real).take(vda_count).enumerate() {
                    self.materialize_out_var(arg, Some(&elem_ty))?;
                    let v = self.codegen_call_arg(arg)?;
                    let v = match elem_dest {
                        Some(dest) => self.box_trait_value(v, dest, arg.span())?,
                        None => v,
                    };
                    if arr_val.is_array_value() {
                        let tmp = self.builder.build_insert_value(arr_val.into_array_value(), v, j as u32, &format!("vararg.{}", j)).unwrap();
                        arr_val = tmp.as_basic_value_enum();
                    }
                }
                if vda_count == 0 {
                    arr_val = arr_llvm_ty.const_zero().into();
                }
            }
            arg_vals.push(arr_val.into());
            for (j, arg) in args.iter().skip(fixed_real + vda_count).enumerate() {
                // tail real-param index = fixed + vda(1) + j → params idx +1 for `this`
                let v = self.codegen_arg_for_param(arg, info, fixed_real + j + 2)?;
                arg_vals.push(v.into());
            }
        } else {
            // Positional prefix, named reorder, default fill (`base` 1 skips `this`).
            arg_vals.extend(self.pack_call_args(args, info, 1)?);
        }
        Ok(arg_vals)
    }

    /// Closed-world dynamic dispatch for a method call on a trait-typed
    /// receiver: evaluate the `{data, tag}` pair and args once, switch on
    /// the tag over the known implementors, call each concrete method with
    /// the shared args, and PHI the results. Sema guaranteed identical
    /// signatures (and public visibility) across implementors.
    fn codegen_trait_method_call(
        &mut self,
        object: &Expr,
        trait_name: &str,
        method: &str,
        args: &[CallArg],
        span: Span,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let pair_val = self.codegen_expr(object)?;
        let pair_st = match pair_val.get_type() {
            BasicTypeEnum::StructType(st) => st,
            _ => {
                return Err(CodegenError {
                    message: format!("trait receiver did not lower to a pair"),
                    span,
                })
            }
        };
        if self.pair_owner_of(pair_st).as_deref() != Some(trait_name) {
            return Err(CodegenError {
                message: format!("trait receiver type mismatch for `{trait_name}`"),
                span,
            });
        }
        let data = self
            .builder
            .build_extract_value(pair_val.into_struct_value(), 0, "trait.data")
            .unwrap()
            .into_pointer_value();
        let tag = self
            .builder
            .build_extract_value(pair_val.into_struct_value(), 1, "trait.tag")
            .unwrap()
            .into_int_value();
        let impls = self.implementors_of(trait_name);
        if impls.is_empty() {
            return Err(CodegenError {
                message: format!("trait `{trait_name}` has no implementors"),
                span,
            });
        }
        // First implementor drives arg packing + result type (identical
        // conventions across all of them, per sema).
        let first_info = self
            .method_func_of(&impls[0], method)
            .map(|(_, info)| info)
            .ok_or(CodegenError {
                message: format!("unknown method `{method}` for trait `{trait_name}`"),
                span,
            })?;
        let arg_vals = self.codegen_method_call_args(args, &first_info, data, span)?;
        let ret_llvm: Option<BasicTypeEnum> = self.llvm_ty_for_sema(&first_info.ret);
        // Single implementor: direct call, no switch needed.
        if impls.len() == 1 {
            let (callee, _) = self.method_func_of(&impls[0], method).ok_or(CodegenError {
                message: format!("unknown method `{method}` for trait `{trait_name}`"),
                span,
            })?;
            let call = self.builder.build_call(callee, &arg_vals, "trait.call").unwrap();
            let vk = call.try_as_basic_value();
            if vk.is_basic() {
                return Ok(vk.basic().unwrap());
            }
            return Ok(self.context.i64_type().const_int(0, false).into());
        }
        let func = self.cur_fn.ok_or(CodegenError {
            message: "trait dispatch outside function".into(),
            span,
        })?;
        let cur_bb = self.builder.get_insert_block().unwrap();
        let merge_bb = self.context.append_basic_block(func, "trait.merge");
        let default_bb = self.context.append_basic_block(func, "trait.unreachable");
        let mut cases: Vec<(
            inkwell::values::IntValue<'ctx>,
            inkwell::basic_block::BasicBlock<'ctx>,
        )> = Vec::new();
        let mut incoming: Vec<(
            BasicValueEnum<'ctx>,
            inkwell::basic_block::BasicBlock<'ctx>,
        )> = Vec::new();
        for cls in &impls {
            let (callee, _) = self.method_func_of(cls, method).ok_or(CodegenError {
                message: format!("unknown method `{method}` for trait `{trait_name}`"),
                span,
            })?;
            let tag_const = *self.class_tags.get(cls).ok_or(CodegenError {
                message: format!("no dynamic tag for `{cls}`"),
                span,
            })?;
            let arm_bb = self.context.append_basic_block(func, "trait.arm");
            cases.push((
                self.context.i64_type().const_int(tag_const, false),
                arm_bb,
            ));
            self.builder.position_at_end(arm_bb);
            let call = self.builder.build_call(callee, &arg_vals, "trait.call").unwrap();
            if ret_llvm.is_some() {
                let vk = call.try_as_basic_value();
                if vk.is_basic() {
                    incoming.push((vk.basic().unwrap(), arm_bb));
                }
            }
            self.builder.build_unconditional_branch(merge_bb).unwrap();
        }
        self.builder.position_at_end(default_bb);
        self.builder.build_unreachable().unwrap();
        self.builder.position_at_end(cur_bb);
        self.builder.build_switch(tag, default_bb, &cases).unwrap();
        self.builder.position_at_end(merge_bb);
        match ret_llvm {
            Some(rt) => {
                let phi = self.builder.build_phi(rt, "trait.result").unwrap();
                let refs: Vec<(&dyn inkwell::values::BasicValue<'ctx>, inkwell::basic_block::BasicBlock<'ctx>)> =
                    incoming.iter().map(|(v, bb)| (v as &dyn inkwell::values::BasicValue<'ctx>, *bb)).collect();
                phi.add_incoming(&refs);
                Ok(phi.as_basic_value())
            }
            None => Ok(self.context.i64_type().const_int(0, false).into()),
        }
    }

    /// Load a field through a trait-typed receiver: switch over
    /// implementors for the field pointer, load the agreed field type.
    fn codegen_trait_field_load(
        &mut self,
        object: &Expr,
        tname: &str,
        field: &str,
        span: Span,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        let field_ptr = self.codegen_trait_field_ptr(object, tname, field, span)?;
        let first = self.implementors_of(tname).into_iter().next().ok_or(CodegenError {
            message: format!("trait `{tname}` has no implementors"),
            span,
        })?;
        let field_ty = self.class_field_llvm_ty(&first, field, span)?;
        Ok(self.builder.build_load(field_ty, field_ptr, field).unwrap())
    }

    /// Field LLVM type for a concrete class (first-implementor layouts are
    /// representative: sema validated identical types across implementors).
    fn class_field_llvm_ty(
        &self,
        class: &str,
        field: &str,
        span: Span,
    ) -> Result<BasicTypeEnum<'ctx>, CodegenError> {
        let fields = self.struct_fields.get(class).ok_or(CodegenError {
            message: format!("unknown class `{class}`"),
            span,
        })?;
        let idx = fields.get(field).ok_or(CodegenError {
            message: format!("trait dispatch: `{class}` has no field `{field}`"),
            span,
        })?;
        let st = self.struct_types.get(class).ok_or(CodegenError {
            message: format!("unknown class `{class}`"),
            span,
        })?;
        Ok(st.get_field_type_at_index(*idx).unwrap())
    }

    /// GEP pointer to `field` through a trait-typed receiver: switch over
    /// implementors (layouts may differ per class), GEP per layout, PHI the
    /// pointers. Serves both field reads and writes.
    fn codegen_trait_field_ptr(
        &mut self,
        object: &Expr,
        tname: &str,
        field: &str,
        span: Span,
    ) -> Result<PointerValue<'ctx>, CodegenError> {
        let pair_val = self.codegen_expr(object)?;
        let data = match pair_val.get_type() {
            BasicTypeEnum::StructType(st)
                if self.pair_owner_of(st).as_deref() == Some(tname) =>
            {
                self.builder
                    .build_extract_value(pair_val.into_struct_value(), 0, "trait.data")
                    .unwrap()
                    .into_pointer_value()
            }
            _ => {
                return Err(CodegenError {
                    message: format!("trait receiver type mismatch for `{tname}`"),
                    span,
                })
            }
        };
        let tag = self
            .builder
            .build_extract_value(pair_val.into_struct_value(), 1, "trait.tag")
            .unwrap()
            .into_int_value();
        let impls = self.implementors_of(tname);
        if impls.is_empty() {
            return Err(CodegenError {
                message: format!("trait `{tname}` has no implementors"),
                span,
            });
        }
        let func = self.cur_fn.ok_or(CodegenError {
            message: "trait field access outside function".into(),
            span,
        })?;
        let cur_bb = self.builder.get_insert_block().unwrap();
        let merge_bb = self.context.append_basic_block(func, "trait.fmerge");
        let default_bb = self.context.append_basic_block(func, "trait.funreachable");
        let mut cases: Vec<(
            inkwell::values::IntValue<'ctx>,
            inkwell::basic_block::BasicBlock<'ctx>,
        )> = Vec::new();
        let mut incoming: Vec<(
            BasicValueEnum<'ctx>,
            inkwell::basic_block::BasicBlock<'ctx>,
        )> = Vec::new();
        for cls in &impls {
            let tag_const = *self.class_tags.get(cls).ok_or(CodegenError {
                message: format!("no dynamic tag for `{cls}`"),
                span,
            })?;
            let fields = self.struct_fields.get(cls).ok_or(CodegenError {
                message: format!("unknown class `{cls}`"),
                span,
            })?;
            let idx = fields.get(field).ok_or(CodegenError {
                message: format!("trait dispatch: `{cls}` has no field `{field}`"),
                span,
            })?;
            let st = self.struct_types.get(cls).ok_or(CodegenError {
                message: format!("unknown class `{cls}`"),
                span,
            })?;
            let arm_bb = self.context.append_basic_block(func, "trait.farm");
            cases.push((
                self.context.i64_type().const_int(tag_const, false),
                arm_bb,
            ));
            self.builder.position_at_end(arm_bb);
            let fptr = self
                .builder
                .build_struct_gep(*st, data, *idx, "trait.field")
                .unwrap();
            incoming.push((fptr.into(), arm_bb));
            self.builder.build_unconditional_branch(merge_bb).unwrap();
        }
        self.builder.position_at_end(default_bb);
        self.builder.build_unreachable().unwrap();
        self.builder.position_at_end(cur_bb);
        self.builder.build_switch(tag, default_bb, &cases).unwrap();
        self.builder.position_at_end(merge_bb);
        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
        let phi = self.builder.build_phi(ptr_ty, "trait.field.ptr").unwrap();
        let refs: Vec<(&dyn inkwell::values::BasicValue<'ctx>, inkwell::basic_block::BasicBlock<'ctx>)> =
            incoming.iter().map(|(v, bb)| (v as &dyn inkwell::values::BasicValue<'ctx>, *bb)).collect();
        phi.add_incoming(&refs);
        Ok(phi.as_basic_value().into_pointer_value())
    }

    fn codegen_function(&mut self, f: &Function) -> Result<(), CodegenError> {
        // Async-6: `async main` splits into a body fn (`__hella_async_main`,
        // normal lowering of the user code) plus a C-ABI `main` wrapper that
        // spawns the body as the root task and joins it.
        if f.name == "main" && f.is_async {
            let body_name = "__hella_async_main";
            let body_f = self.funcs.get(body_name).map(|(fv, _)| *fv).unwrap();
            // Codegen the user body into `__hella_async_main` (as an ordinary
            // sync function).
            let mut body_copy = f.clone();
            body_copy.name = body_name.to_string();
            body_copy.is_async = false;
            self.codegen_function(&body_copy)?;
            return self.codegen_async_main_wrapper(body_f, f);
        }
        let (func, info) =
            self.funcs.get(&f.name).cloned().ok_or(CodegenError {
                message: format!("undeclared func {}", f.name),
                span: f.name_span,
            })?;
        // NOTE (real stdlib): `print`-family names lower as ordinary calls.
        // `std::io` wrappers are compiled like any other Hella function.
        self.cur_fn = Some(func);
        self.cur_is_main = f.name == "main";
        self.main_args_alloca = None;
        self.main_argc_alloca = None;
        let entry = self.context.append_basic_block(func, "entry");
        self.builder.position_at_end(entry);

        self.vars.push(HashMap::new());
        self.own_slots.push(Vec::new());
        self.scope_dtors.push(Vec::new());
        // Every `main` captures argv[0] (the program name) into the
        // `__hella_argv0` global for `__hella_progname()` — all main forms
        // declare `(i32 argc, ptr argv)` at LLVM level, so this works with
        // or without a Hella-level `args` parameter.
        if f.name == "main" && !self.funcs.contains_key("__hella_async_main") {
            let i64_ty = self.context.i64_type();
            let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
            let argc = self.builder.build_int_s_extend(func.get_nth_param(0).unwrap().into_int_value(), i64_ty, "argc64").unwrap();
            let argv = func.get_nth_param(1).unwrap().into_pointer_value();
            let has_argv0 = self.builder.build_int_compare(IntPredicate::SGT, argc, i64_ty.const_zero(), "argv0.has").unwrap();
            let slot0 = unsafe { self.builder.build_gep(ptr_ty, argv, &[i64_ty.const_zero()], "argv0.slot").unwrap() };
            let s0 = self.builder.build_load(ptr_ty, slot0, "argv0.str").unwrap();
            let v0 = self.builder.build_select(has_argv0, s0, ptr_ty.const_null().into(), "argv0").unwrap();
            self.builder.build_store(self.argv0_global(), v0).unwrap();
        }
        // Special handling for `int main(string[] args)`: the LLVM function
        // is C `i32 (i32 argc, ptr argv)`; `args` is a local `[16 x string]`
        // filled with argv[1..] (program name excluded, C#/Java-style),
        // capped at 16 entries, remainder null.
        let is_main_with_args = f.name == "main"
            && f.params.len() == 1
            && f.params[0].name == "args"
            && matches!(&f.params[0].ty, crate::ast::Type::Array(el, _) if matches!(el.as_ref(), crate::ast::Type::String(_)));
        if is_main_with_args {
            let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
            let args_arr_ty = ptr_ty.array_type(16);
            let args_ty: BasicTypeEnum<'ctx> = args_arr_ty.into();
            let args_alloca = self.create_entry_block_alloca("args", args_ty);
            let zero: BasicValueEnum<'ctx> = args_arr_ty.const_zero().into();
            self.builder.build_store(args_alloca, zero).unwrap();
            self.vars.last_mut().unwrap().insert("args".to_string(), (args_alloca, args_ty));
            // count = min(max(argc - 1, 0), 16)
            let i64_ty = self.context.i64_type();
            let argc = self.builder.build_int_s_extend(func.get_nth_param(0).unwrap().into_int_value(), i64_ty, "argc64").unwrap();
            let argv = func.get_nth_param(1).unwrap().into_pointer_value();
            let argc_minus_1 = self.builder.build_int_sub(argc, i64_ty.const_int(1, false), "argc.m1").unwrap();
            let is_pos = self.builder.build_int_compare(IntPredicate::SGT, argc_minus_1, i64_ty.const_int(0, false), "argc.pos").unwrap();
            let nonneg = self.builder.build_select(is_pos, argc_minus_1, i64_ty.const_zero(), "argc.nonneg").unwrap();
            let nonneg = nonneg.into_int_value();
            let over = self.builder.build_int_compare(IntPredicate::SGT, nonneg, i64_ty.const_int(16, false), "argc.over").unwrap();
            let count = self.builder.build_select(over, i64_ty.const_int(16, false), nonneg, "argc.count").unwrap().into_int_value();
            // Stash the true argument count where `args.len()` and friends
            // can load it (the array itself stays 16 statically-sized slots).
            let argc_alloca = self.create_entry_block_alloca("__hella_argc", i64_ty.into());
            self.builder.build_store(argc_alloca, count).unwrap();
            self.main_args_alloca = Some(args_alloca);
            self.main_argc_alloca = Some(argc_alloca);
            // idx loop: args[idx] = argv[idx + 1]
            let idx_ptr = self.create_entry_block_alloca("__argv_idx", i64_ty.into());
            self.builder.build_store(idx_ptr, i64_ty.const_zero()).unwrap();
            let copy_cond = self.context.append_basic_block(func, "argv.copy.cond");
            let copy_body = self.context.append_basic_block(func, "argv.copy.body");
            let copy_done = self.context.append_basic_block(func, "argv.copy.done");
            self.builder.build_unconditional_branch(copy_cond).unwrap();
            self.builder.position_at_end(copy_cond);
            let idx = self.builder.build_load(i64_ty, idx_ptr, "argv.idx").unwrap().into_int_value();
            let more = self.builder.build_int_compare(IntPredicate::SLT, idx, count, "argv.more").unwrap();
            self.builder.build_conditional_branch(more, copy_body, copy_done).unwrap();
            self.builder.position_at_end(copy_body);
            let src_idx = self.builder.build_int_add(idx, i64_ty.const_int(1, false), "argv.src").unwrap();
            let slot_ptr = unsafe { self.builder.build_gep(ptr_ty, argv, &[src_idx], "argv.slot").unwrap() };
            let s = self.builder.build_load(ptr_ty, slot_ptr, "argv.str").unwrap();
            let dst_ptr = unsafe { self.builder.build_gep(args_arr_ty, args_alloca, &[i64_ty.const_int(0, false), idx], "args.slot").unwrap() };
            self.builder.build_store(dst_ptr, s).unwrap();
            let next = self.builder.build_int_add(idx, i64_ty.const_int(1, false), "argv.next").unwrap();
            self.builder.build_store(idx_ptr, next).unwrap();
            self.builder.build_unconditional_branch(copy_cond).unwrap();
            self.builder.position_at_end(copy_done);
        } else {
            for (i, param) in f.params.iter().enumerate() {
                let param_val = func.get_nth_param(i as u32).unwrap();
                if param.mode != ParamMode::None {
                    let inner_ty = self.llvm_ty_for(&param.ty);
                    let ptr = param_val.into_pointer_value();
                    self.vars.last_mut().unwrap().insert(param.name.clone(), (ptr, inner_ty));
                } else if param.is_variadic {
                // `...T vda` where `vda` is `T[]` array, `... vda` derived from previous
                let elem_ty = self.llvm_ty_for(&param.ty);
                // For derived `__derived__`, elem_ty is placeholder, use previous param's type
                let actual_elem_ty = if param.ty.name() == "__derived__" {
                    if i > 0 {
                        self.llvm_ty_for(&f.params[i-1].ty)
                    } else { elem_ty }
                } else { elem_ty };
                let arr_ty = match actual_elem_ty {
                    ty if ty.is_int_type() => self.context.i64_type().array_type(16).into(),
                    ty if ty.is_pointer_type() => ty.into_pointer_type().array_type(16).into(),
                    _ => actual_elem_ty,
                };
                // For variadic, the param is already an array value (passed as array), need alloca for it
                let alloca = self.create_entry_block_alloca(&param.name, arr_ty);
                // param_val is array value for `...T vda` case, not pointer, so store it
                // For `...T vda` where `vda` is `T[]`, the LLVM param is array type, so param_val is array value
                self.builder.build_store(alloca, param_val).unwrap();
                self.vars.last_mut().unwrap().insert(param.name.clone(), (alloca, arr_ty));
                } else {
                let llvm_ty = self.llvm_ty_for(&param.ty);
                let alloca = self.create_entry_block_alloca(&param.name, llvm_ty);
                    self.builder.build_store(alloca, param_val).unwrap();
                    self.vars.last_mut().unwrap().insert(param.name.clone(), (alloca, llvm_ty));
                        if matches!(&param.ty, Type::Vec { .. }) { self.vec_vars.insert(param.name.clone()); }
                        if matches!(&param.ty, Type::Map { .. }) { self.map_vars.insert(param.name.clone()); }
                        if matches!(&param.ty, Type::String(_)) { self.string_vars.insert(param.name.clone()); }
                        self.track_unsigned_var(&param.name, &param.ty);
                    // Track `own` params for destruction at function exit.
                    self.track_own_param(alloca, &param.ty);
                    // Track params needing destructors (user dtors,
                    // structural `own`-field destruction, or arrays of
                    // either) the same way.
                    self.track_dtor_slot(alloca, &param.ty, llvm_ty);
                    // Vectors/maps with owned element types: register for
                    // container-dtor invocation at scope exit.
                    self.track_container_dtor(alloca, &param.ty, llvm_ty);
                }
            }

        }
        if f.name == "main" {
            self.emit_program_startup()?;
        }
        let always_returns = self.codegen_block(&f.body)?;

        if !always_returns
            && self
                .builder
                .get_insert_block()
                .unwrap()
                .get_terminator()
                .is_none()
        {
            // Destroy owned params still live at fall-through exit
            self.emit_current_scope_owns();
            self.emit_current_scope_dtors();
            if self.cur_is_main {
                // Program end: destroy owning globals (reverse declared).
                self.emit_global_dtors();
                let zero = self.context.i32_type().const_int(0, false);
                self.builder.build_return(Some(&zero)).unwrap();
            } else if info.ret == crate::sema::Ty::Void {
                self.builder.build_return(None).unwrap();
            } else {
                let zero: BasicValueEnum = match info.ret {
                    crate::sema::Ty::Int => {
                        self.context.i64_type().const_int(0, false).into()
                    }
                    crate::sema::Ty::UInt => {
                        self.context.i64_type().const_int(0, false).into()
                    }
                    crate::sema::Ty::SizedInt { bits, .. } => {
                        self.llvm_int_for_bits(bits).const_int(0, false).into()
                    }
                    crate::sema::Ty::Bool => {
                        self.context.bool_type().const_int(0, false).into()
                    }
                    crate::sema::Ty::Char => {
                        self.context.i32_type().const_int(0, false).into()
                    }
                    crate::sema::Ty::String => self
                        .context
                        .ptr_type(inkwell::AddressSpace::default())
                        .const_null()
                        .into(),
                    crate::sema::Ty::Struct(ref n) => {
                        if let Some(pair) = self.trait_pair_of(n) {
                            pair.const_zero().into()
                        } else {
                            self.struct_types.get(n).unwrap().const_zero().into()
                        }
                    }
                    crate::sema::Ty::Own(inner) => {
                        self.own_pair_type(inner.as_ref()).const_zero().into()
                    }
                    // `task<T>` (Async-6): null handle (never observed: sema
                    // rejects un-awaited tasks, so this is unreachable).
                    crate::sema::Ty::Task(_) => self
                        .context
                        .ptr_type(inkwell::AddressSpace::default())
                        .const_null()
                        .into(),
                    crate::sema::Ty::Array(_) => self
                        .context
                        .i64_type()
                        .array_type(16)
                        .const_zero()
                        .into(),
                    crate::sema::Ty::FixedArray { elem: ref elem, size: ref size } => {
                        let n = size.unwrap_or(16) as u32;
                        match self.llvm_ty_for_sema(elem.as_ref()) {
                            Some(BasicTypeEnum::IntType(it)) => it.array_type(n).const_zero().into(),
                            Some(BasicTypeEnum::FloatType(ft)) => ft.array_type(n).const_zero().into(),
                            Some(BasicTypeEnum::PointerType(pt)) => pt.array_type(n).const_zero().into(),
                            Some(BasicTypeEnum::StructType(st)) => st.array_type(n).const_zero().into(),
                            Some(BasicTypeEnum::ArrayType(at)) => at.array_type(n).const_zero().into(),
                            _ => self.context.i64_type().array_type(n).const_zero().into(),
                        }
                    },
                    crate::sema::Ty::Vec(ref elem) => {
                        let inner = match elem.as_ref() {
                            crate::sema::Ty::Any => self.context.i64_type().into(),
                            _ => self.llvm_ty_for_sema(elem).unwrap_or_else(|| self.context.i64_type().into()),
                        };
                        self.vec_struct_ty(inner).const_zero().into()
                    },
                    crate::sema::Ty::Map { key: ref key, value: ref value } => {
                        let k = match key.as_ref() {
                            crate::sema::Ty::Any => self.context.i64_type().into(),
                            _ => self.llvm_ty_for_sema(key.as_ref()).unwrap_or_else(|| self.context.i64_type().into()),
                        };
                        let v = match value.as_ref() {
                            crate::sema::Ty::Any => self.context.ptr_type(inkwell::AddressSpace::default()).into(),
                            _ => self.llvm_ty_for_sema(value.as_ref()).unwrap_or_else(|| self.context.i64_type().into()),
                        };
                        self.map_struct_ty(k, v).const_zero().into()
                    },
                    crate::sema::Ty::Pointer(_) => self
                        .context
                        .ptr_type(inkwell::AddressSpace::default())
                        .const_null()
                        .into(),
                    crate::sema::Ty::Optional(ref el) => {
                        let inner = self.llvm_ty_for_sema(el).unwrap();
                        self.context
                            .struct_type(
                                &[
                                    inner.into(),
                                    self.context.bool_type().into(),
                                ],
                                false,
                            )
                            .const_zero()
                            .into()
                    }
                    crate::sema::Ty::Void => unreachable!(),
                crate::sema::Ty::Enum(ref n) => self.enum_types.get(n).unwrap().const_zero().into(),
                crate::sema::Ty::Float => self.context.f32_type().const_float(0.0).into(),
                crate::sema::Ty::Double => self.context.f64_type().const_float(0.0).into(),
                    crate::sema::Ty::Generic(_, _) => self.context.ptr_type(inkwell::AddressSpace::default()).const_null().into(),
                    crate::sema::Ty::Tuple(ref tys) => self.tuple_struct_ty(tys).map(|st| st.const_zero().into()).unwrap_or_else(|| self.context.ptr_type(inkwell::AddressSpace::default()).const_null().into()),
                    crate::sema::Ty::Any => self.context.ptr_type(inkwell::AddressSpace::default()).const_null().into(),
                crate::sema::Ty::Function(_, _) => self.context.ptr_type(inkwell::AddressSpace::default()).const_null().into(),
                };
                self.builder.build_return(Some(&zero)).unwrap();
            }
        }

        self.own_slots.pop();
        self.scope_dtors.pop();
        self.vars.pop();
        self.cur_fn = None;
        self.cur_is_main = false;
        if !func.verify(true) {
            return Err(CodegenError {
                message: format!("function {} failed verification", f.name),
                span: f.span,
            });
        }
        Ok(())
    }

    /// Default value for an implicit function return (`None` for `void`).
    /// Used to terminate bodies that fall off the end without an explicit
    /// `return` (class methods and extension functions share it).
    fn default_return_value(&self, ret: &crate::sema::Ty) -> Option<BasicValueEnum<'ctx>> {
        match ret {
            crate::sema::Ty::Void => None,
            crate::sema::Ty::Int => Some(self.context.i64_type().const_int(0, false).into()),
            crate::sema::Ty::UInt => Some(self.context.i64_type().const_int(0, false).into()),
            crate::sema::Ty::SizedInt { bits, .. } => Some(self.llvm_int_for_bits(*bits).const_int(0, false).into()),
            crate::sema::Ty::Bool => Some(self.context.bool_type().const_int(0, false).into()),
            crate::sema::Ty::Char => Some(self.context.i32_type().const_int(0, false).into()),
            crate::sema::Ty::String => Some(self.context.ptr_type(inkwell::AddressSpace::default()).const_null().into()),
            crate::sema::Ty::Struct(n) => Some(
                self.pair_types
                    .get(n)
                    .map(|pair| pair.const_zero().into())
                    .unwrap_or_else(|| self.struct_types.get(n).unwrap().const_zero().into()),
            ),
            crate::sema::Ty::Own(inner) => {
                Some(self.own_pair_type(inner.as_ref()).const_zero().into())
            }
            crate::sema::Ty::Task(_) => Some(self.context.ptr_type(inkwell::AddressSpace::default()).const_null().into()),
            crate::sema::Ty::Array(_) => Some(self.context.i64_type().array_type(16).const_zero().into()),
            crate::sema::Ty::FixedArray { elem, size } => {
                let n = size.unwrap_or(16) as u32;
                match self.llvm_ty_for_sema(elem.as_ref()) {
                    Some(BasicTypeEnum::IntType(it)) => Some(it.array_type(n).const_zero().into()),
                    Some(BasicTypeEnum::FloatType(ft)) => Some(ft.array_type(n).const_zero().into()),
                    Some(BasicTypeEnum::PointerType(pt)) => Some(pt.array_type(n).const_zero().into()),
                    Some(BasicTypeEnum::StructType(st)) => Some(st.array_type(n).const_zero().into()),
                    Some(BasicTypeEnum::ArrayType(at)) => Some(at.array_type(n).const_zero().into()),
                    _ => Some(self.context.i64_type().array_type(n).const_zero().into()),
                }
            },
            crate::sema::Ty::Vec(elem) => {
                let inner = match elem.as_ref() {
                    crate::sema::Ty::Any => self.context.i64_type().into(),
                    _ => self.llvm_ty_for_sema(elem).unwrap_or_else(|| self.context.i64_type().into()),
                };
                Some(self.vec_struct_ty(inner).const_zero().into())
            },
            crate::sema::Ty::Map { key, value } => {
                let k = match key.as_ref() {
                    crate::sema::Ty::Any => self.context.i64_type().into(),
                    _ => self.llvm_ty_for_sema(key.as_ref()).unwrap_or_else(|| self.context.i64_type().into()),
                };
                let v = match value.as_ref() {
                    crate::sema::Ty::Any => self.context.ptr_type(inkwell::AddressSpace::default()).into(),
                    _ => self.llvm_ty_for_sema(value.as_ref()).unwrap_or_else(|| self.context.i64_type().into()),
                };
                Some(self.map_struct_ty(k, v).const_zero().into())
            },
            crate::sema::Ty::Pointer(_) => Some(self.context.ptr_type(inkwell::AddressSpace::default()).const_null().into()),
            crate::sema::Ty::Optional(el) => {
                let inner = self.llvm_ty_for_sema(el).unwrap();
                Some(self.context.struct_type(&[inner.into(), self.context.bool_type().into()], false).const_zero().into())
            }
            crate::sema::Ty::Enum(n) => Some(self.enum_types.get(n).unwrap().const_zero().into()),
            crate::sema::Ty::Float => Some(self.context.f32_type().const_float(0.0).into()),
            crate::sema::Ty::Double => Some(self.context.f64_type().const_float(0.0).into()),
            crate::sema::Ty::Generic(_, _) => Some(self.context.ptr_type(inkwell::AddressSpace::default()).const_null().into()),
            crate::sema::Ty::Tuple(tys) => Some(self.tuple_struct_ty(&tys).map(|st| st.const_zero().into()).unwrap_or_else(|| self.context.ptr_type(inkwell::AddressSpace::default()).const_null().into())),
            crate::sema::Ty::Any => Some(self.context.ptr_type(inkwell::AddressSpace::default()).const_null().into()),
            crate::sema::Ty::Function(_, _) => Some(self.context.ptr_type(inkwell::AddressSpace::default()).const_null().into()),
        }
    }

    fn codegen_class_method(&mut self, class: &ClassDecl, method: &Function) -> Result<(), CodegenError> {
        let methods = self.class_methods.get(&class.name).ok_or(CodegenError{message: format!("unknown class {}", class.name), span: class.name_span})?;
        let (func, info) = methods.get(&method.name).cloned().ok_or(CodegenError{message: format!("unknown method {}", method.name), span: method.name_span})?;
        self.cur_fn = Some(func);
        self.cur_class = Some(class.name.clone());
        self.cur_is_main = false;
        let entry = self.context.append_basic_block(func, "entry");
        self.builder.position_at_end(entry);
        self.vars.push(HashMap::new());
        self.own_slots.push(Vec::new());
        self.scope_dtors.push(Vec::new());
        let this_ty: BasicTypeEnum<'ctx> = self.context.ptr_type(inkwell::AddressSpace::default()).into();
        let this_param = func.get_nth_param(0).unwrap();
        let this_alloca = self.create_entry_block_alloca("this", this_ty);
        self.builder.build_store(this_alloca, this_param).unwrap();
        self.vars.last_mut().unwrap().insert("this".to_string(), (this_alloca, this_ty));
        for (i, param) in method.params.iter().enumerate() {
            // Variadic `...T vda` -> `T[]` array type, `... vda` derived from previous
            let llvm_ty = if param.is_variadic {
                if param.ty.name() == "__derived__" {
                    if i == 0 {
                        self.context.i64_type().array_type(16).into()
                    } else {
                        let prev_ty = self.llvm_ty_for(&method.params[i-1].ty);
                        match prev_ty {
                            ty if ty.is_int_type() => self.context.i64_type().array_type(16).into(),
                            ty if ty.is_pointer_type() => ty.into_pointer_type().array_type(16).into(),
                            ty if ty.is_struct_type() => ty.into_struct_type().array_type(16).into(),
                            _ => prev_ty,
                        }
                    }
                } else {
                    let elem_ty = self.llvm_ty_for(&param.ty);
                    match elem_ty {
                        ty if ty.is_int_type() => self.context.i64_type().array_type(16).into(),
                        ty if ty.is_pointer_type() => ty.into_pointer_type().array_type(16).into(),
                        ty if ty.is_struct_type() => ty.into_struct_type().array_type(16).into(),
                        _ => elem_ty,
                    }
                }
            } else {
                self.llvm_ty_for(&param.ty)
            };
            let param_val = func.get_nth_param((i+1) as u32).unwrap();
            if param.mode != ParamMode::None {
                // `ref`/`out`: the caller passed a pointer; use it directly.
                let inner_ty = self.llvm_ty_for(&param.ty);
                let ptr = param_val.into_pointer_value();
                self.vars.last_mut().unwrap().insert(param.name.clone(), (ptr, inner_ty));
            } else {
                let alloca = self.create_entry_block_alloca(&param.name, llvm_ty);
                self.builder.build_store(alloca, param_val).unwrap();
                self.vars.last_mut().unwrap().insert(param.name.clone(), (alloca, llvm_ty));
                self.track_own_param(alloca, &param.ty);
                self.track_dtor_slot(alloca, &param.ty, llvm_ty);
                if matches!(&param.ty, Type::Vec { .. }) || matches!(&param.ty, Type::Map { .. }) {
                    self.track_container_dtor(alloca, &param.ty, llvm_ty);
                }
            }
                        if matches!(&param.ty, Type::Map { .. }) { self.map_vars.insert(param.name.clone()); }
                        if matches!(&param.ty, Type::String(_)) { self.string_vars.insert(param.name.clone()); }
                        self.track_unsigned_var(&param.name, &param.ty);
        }
        let always_returns = self.codegen_block(&method.body)?;
        if !always_returns && self.builder.get_insert_block().unwrap().get_terminator().is_none() {
            // Destroy owned params still live at fall-through exit (early
            // `return` already ran `emit_all_owns`).
            self.emit_current_scope_owns();
            self.emit_current_scope_dtors();
            match self.default_return_value(&info.ret) {
                Some(zero) => { self.builder.build_return(Some(&zero)).unwrap(); }
                None => { self.builder.build_return(None).unwrap(); }
            }
        }
        self.own_slots.pop();
        self.scope_dtors.pop();
        self.vars.pop();
        self.cur_fn = None;
        self.cur_class = None;
        if !func.verify(true) {
            return Err(CodegenError{message: format!("method {}::{} failed verification", class.name, method.name), span: method.span});
        }
        Ok(())
    }

    fn codegen_constructor(&mut self, class: &ClassDecl, ctor: &ConstructorDecl, idx: usize) -> Result<(), CodegenError> {
        let mangled = format!("{}__ctor{}", class.name, if class.constructors.len()>1 { format!("{}", idx)} else { "".to_string()});
        let func = self.module.get_function(&mangled).ok_or(CodegenError{message: format!("ctor not declared {}", mangled), span: ctor.span})?;
        self.cur_fn = Some(func);
        self.cur_class = Some(class.name.clone());
        let entry = self.context.append_basic_block(func, "entry");
        self.builder.position_at_end(entry);
        self.vars.push(HashMap::new());
        self.own_slots.push(Vec::new());
        self.scope_dtors.push(Vec::new());
        let this_ty: BasicTypeEnum<'ctx> = self.context.ptr_type(inkwell::AddressSpace::default()).into();
        let this_param = func.get_nth_param(0).unwrap();
        let this_alloca = self.create_entry_block_alloca("this", this_ty);
        self.builder.build_store(this_alloca, this_param).unwrap();
        self.vars.last_mut().unwrap().insert("this".to_string(), (this_alloca, this_ty));
        for (i, param) in ctor.params.iter().enumerate() {
            let llvm_ty = if param.is_variadic {
                if param.ty.name() == "__derived__" {
                    if i == 0 {
                        self.context.i64_type().array_type(16).into()
                    } else {
                        let prev_ty = self.llvm_ty_for(&ctor.params[i-1].ty);
                        match prev_ty {
                            ty if ty.is_int_type() => self.context.i64_type().array_type(16).into(),
                            ty if ty.is_pointer_type() => ty.into_pointer_type().array_type(16).into(),
                            _ => prev_ty,
                        }
                    }
                } else {
                    let elem_ty = self.llvm_ty_for(&param.ty);
                    match elem_ty {
                        ty if ty.is_int_type() => self.context.i64_type().array_type(16).into(),
                        ty if ty.is_pointer_type() => ty.into_pointer_type().array_type(16).into(),
                        _ => elem_ty,
                    }
                }
            } else {
                self.llvm_ty_for(&param.ty)
            };
            let val = func.get_nth_param((i+1) as u32).unwrap();
            if param.mode != ParamMode::None {
                // `ref`/`out`: the caller passed a pointer; use it directly.
                let inner_ty = self.llvm_ty_for(&param.ty);
                let ptr = val.into_pointer_value();
                self.vars.last_mut().unwrap().insert(param.name.clone(), (ptr, inner_ty));
                } else {
                let alloca = self.create_entry_block_alloca(&param.name, llvm_ty);
                self.builder.build_store(alloca, val).unwrap();
                self.vars.last_mut().unwrap().insert(param.name.clone(), (alloca, llvm_ty));
                self.track_own_param(alloca, &param.ty);
                self.track_dtor_slot(alloca, &param.ty, llvm_ty);
            }
                        if matches!(&param.ty, Type::Vec { .. }) { self.vec_vars.insert(param.name.clone()); }
                        if matches!(&param.ty, Type::Map { .. }) { self.map_vars.insert(param.name.clone()); }
                        if matches!(&param.ty, Type::String(_)) { self.string_vars.insert(param.name.clone()); }
                        self.track_unsigned_var(&param.name, &param.ty);
        }
        // `initialize` sugar: this.field = param for each param matching a field
        // EBNF §22: initialize is sugar for this.field = field
        if let Some(field_map) = self.struct_fields.get(&class.name).cloned() {
            for param in &ctor.params {
                if let Some(&idx) = field_map.get(&param.name) {
                    let st = *self.struct_types.get(&class.name).unwrap();
                    // this pointer
                    let (this_alloca, this_ty) = self.lookup_var("this").unwrap();
                    let this_ptr = self.builder.build_load(this_ty, this_alloca, "this.load").unwrap().into_pointer_value();
                    let field_ptr = self.builder.build_struct_gep(st, this_ptr, idx, &format!("init.{}", param.name)).unwrap();
                    let (param_ptr, param_ty) = self.lookup_var(&param.name).unwrap();
                    let val = self.builder.build_load(param_ty, param_ptr, &param.name).unwrap();
                    self.builder.build_store(field_ptr, val).unwrap();
                    // Move into the field: null owned content in the param
                    // slot (mirrors sema poisoning; primitives unaffected).
                    self.null_own_fields(param_ptr, param_ty, 0);
                }
            }
        }
        if let Some(body) = &ctor.body {
            let _ = self.codegen_block(body)?;
        }
        if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
            self.emit_current_scope_owns();
            self.emit_current_scope_dtors();
            self.builder.build_return(None).unwrap();
        }
        self.own_slots.pop();
        self.scope_dtors.pop();
        self.vars.pop();
        self.cur_fn = None;
        self.cur_class = None;
        if !func.verify(true) { return Err(CodegenError{message: format!("ctor {} failed verify", class.name), span: ctor.span}); }
        Ok(())
    }

    fn codegen_destructor(&mut self, class: &ClassDecl, dtor: &DestructorDecl, idx: usize) -> Result<(), CodegenError> {
        let mangled = format!("{}__dtor{}", class.name, if class.destructors.len()>1 { format!("{}", idx)} else { "".to_string()});
        let func = self.module.get_function(&mangled).ok_or(CodegenError{message: format!("dtor not declared {}", mangled), span: dtor.span})?;
        self.cur_fn = Some(func);
        self.cur_class = Some(class.name.clone());
        let entry = self.context.append_basic_block(func, "entry");
        self.builder.position_at_end(entry);
        self.vars.push(HashMap::new());
        let this_ty: BasicTypeEnum<'ctx> = self.context.ptr_type(inkwell::AddressSpace::default()).into();
        let this_param = func.get_nth_param(0).unwrap();
        let this_alloca = self.create_entry_block_alloca("this", this_ty);
        self.builder.build_store(this_alloca, this_param).unwrap();
        self.vars.last_mut().unwrap().insert("this".to_string(), (this_alloca, this_ty));
        let _ = self.codegen_block(&dtor.body)?;
        if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
            self.builder.build_return(None).unwrap();
        }
        self.vars.pop();
        self.cur_fn = None;
        self.cur_class = None;
        if !func.verify(true) { return Err(CodegenError{message: format!("dtor {} failed verify", class.name), span: dtor.span}); }
        Ok(())
    }

    /// Class name for a declared local type if it has a destructor.
    fn dtor_class_for_ty(&self, ty: &Type) -> Option<String> {
        let name = match ty {
            Type::Named(n, _) => n.rsplit("::").next().unwrap_or(n).to_string(),
            Type::Generic(n, _, _) => n.rsplit("::").next().unwrap_or(n).to_string(),
            _ => return None,
        };
        if self.class_destructors.contains_key(&name) {
            Some(name)
        } else if self.trait_names.contains(&name)
            && self
                .implementors_of(&name)
                .iter()
                .any(|c| self.class_destructors.contains_key(c))
        {
            // Trait-typed slot: destroy via tag dispatch as long as at
            // least one implementor has a destructor. (The name stored is
            // the trait; `emit_dtor_call` dispatches on it.)
            Some(name)
        } else {
            None
        }
    }

    /// Destructor name for a slot of AST type: user destructors first
    /// (`dtor_class_for_ty`), else structural destruction for structs with
    /// transitive `own` fields. Powers scope-exit, assignment-overwrite,
    /// and parameter tracking uniformly.
    fn dtor_name_for_ast_ty(&self, ty: &Type) -> Option<String> {
        if let Some(n) = self.dtor_class_for_ty(ty) {
            return Some(n);
        }
        let base = match ty {
            Type::Named(n, _) | Type::Generic(n, _, _) => n.rsplit("::").next().unwrap_or(n),
            _ => return None,
        };
        if self.struct_needs_field_destroy(base) {
            Some(base.to_string())
        } else {
            None
        }
    }

    /// Register a freshly allocated slot for scope-exit destruction: the
    /// whole slot when it needs it, plus one entry per element for fixed
    /// arrays of owned structs (static indices, so the shared
    /// `(alloca, name)` entries keep working).
    fn track_dtor_slot(
        &mut self,
        alloca: PointerValue<'ctx>,
        ast_ty: &Type,
        llvm_ty: BasicTypeEnum<'ctx>,
    ) {
        if let Some(name) = self.dtor_name_for_ast_ty(ast_ty) {
            if let Some(top) = self.scope_dtors.last_mut() {
                top.push((alloca, name));
            }
        }
        self.track_dtor_array_elems(alloca, ast_ty, llvm_ty);
        // Vectors/maps with owned element types: register for container-dtor
        // invocation at scope exit (the generated `__container_dtor_N` walks
        // the live range and destroys each element that needs it).
        self.track_container_dtor(alloca, ast_ty, llvm_ty);
    }

    /// Register a vec/map slot for scope-exit container destruction when the
    /// element/key/value type transitively owns heap data. No-op for primitive
    /// element types (nothing to free). The stored string keys
    /// `emit_dtor_call`, which dispatches to the generated `__container_dtor_N`.
    fn track_container_dtor(
        &mut self,
        alloca: PointerValue<'ctx>,
        ast_ty: &Type,
        llvm_ty: BasicTypeEnum<'ctx>,
    ) {
        if let Type::Vec { elem, .. } = ast_ty {
            let elem_llvm = self.llvm_ty_for(elem);
            let need = matches!(elem_llvm, BasicTypeEnum::StructType(_) | BasicTypeEnum::ArrayType(_));
            if need {
                let key = self.container_dtor_for_vec(elem_llvm);
                if let Some(top) = self.scope_dtors.last_mut() {
                    top.push((alloca, key));
                }
            }
        } else if let Type::Map { key, value, .. } = ast_ty {
            let key_llvm = self.llvm_ty_for(key);
            let val_llvm = self.llvm_ty_for(value);
            let need = matches!(key_llvm, BasicTypeEnum::StructType(_) | BasicTypeEnum::ArrayType(_))
                || matches!(val_llvm, BasicTypeEnum::StructType(_) | BasicTypeEnum::ArrayType(_));
            if need {
                let key = self.container_dtor_for_map(key_llvm, val_llvm);
                if let Some(top) = self.scope_dtors.last_mut() {
                    top.push((alloca, key));
                }
            }
        }
    }

    fn emit_dtor_call(&mut self, alloca: PointerValue<'ctx>, class_name: &str) {
        // Generated container destructors share the entry shape.
        if let Some(func) = self.container_dtors.get(class_name).cloned() {
            let arg: inkwell::values::BasicMetadataValueEnum = alloca.into();
            let _ = self.builder.build_call(func, &[arg], "container.dtor.call");
            return;
        }
        if self.trait_names.contains(class_name) {
            self.emit_trait_dtor_call(alloca, class_name);
            return;
        }
        if let Some(dtors) = self.class_destructors.get(class_name).cloned() {
            for (func, _) in dtors {
                let arg: inkwell::values::BasicMetadataValueEnum = alloca.into();
                let _ = self.builder.build_call(func, &[arg], "dtor.call");
            }
        }
        // Structural destruction for `own` fields (structs and classes
        // alike), after any user destructor body (C++ member order).
        if self.struct_needs_field_destroy(class_name) {
            self.emit_struct_field_destroy(alloca, class_name);
        }
    }

    /// Generated destructor for a vector slot holding owned elements:
    /// destroys `buf[0..len)`. Memoized by element type.
    fn container_dtor_for_vec(
        &mut self,
        elem_ty: BasicTypeEnum<'ctx>,
    ) -> String {
        let key = format!("vec:{:?}", elem_ty);
        if self.container_dtors.contains_key(&key) {
            return key;
        }
        let name = format!("__container_dtor_{}", self.container_dtors.len());
        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
        let func = self.module.add_function(&name, self.context.void_type().fn_type(&[ptr_ty.into()], false), None);
        self.container_dtors.insert(key.clone(), func);
        let prev_fn = self.cur_fn;
        let prev_block = self.builder.get_insert_block();
        let entry = self.context.append_basic_block(func, "entry");
        self.builder.position_at_end(entry);
        self.cur_fn = Some(func);
        let slot = func.get_nth_param(0).unwrap().into_pointer_value();
        let vec_st = self.vec_struct_ty(elem_ty);
        let i64_ty = self.context.i64_type();
        let len_ptr = self.builder.build_struct_gep(vec_st, slot, 1, "cvec.len.ptr").unwrap();
        let len = self.builder.build_load(i64_ty, len_ptr, "cvec.len").unwrap().into_int_value();
        let buf_ptr = self.builder.build_struct_gep(vec_st, slot, 0, "cvec.buf").unwrap();
        let buf_arr = match elem_ty {
            BasicTypeEnum::IntType(it) => it.array_type(Self::VEC_CAP),
            BasicTypeEnum::FloatType(ft) => ft.array_type(Self::VEC_CAP),
            BasicTypeEnum::PointerType(pt) => pt.array_type(Self::VEC_CAP),
            BasicTypeEnum::StructType(st) => st.array_type(Self::VEC_CAP),
            BasicTypeEnum::ArrayType(at) => at.array_type(Self::VEC_CAP),
            _ => self.context.i64_type().array_type(Self::VEC_CAP),
        };
        let idx_ptr = self.create_entry_block_alloca("__cdtor_idx", i64_ty.into());
        self.builder.build_store(idx_ptr, i64_ty.const_zero()).unwrap();
        let cond_bb = self.context.append_basic_block(func, "cdtor.cond");
        let body_bb = self.context.append_basic_block(func, "cdtor.body");
        let done_bb = self.context.append_basic_block(func, "cdtor.done");
        self.builder.build_unconditional_branch(cond_bb).unwrap();
        self.builder.position_at_end(cond_bb);
        let idx = self.builder.build_load(i64_ty, idx_ptr, "cdtor.idx").unwrap().into_int_value();
        let more = self.builder.build_int_compare(IntPredicate::SLT, idx, len, "cdtor.more").unwrap();
        self.builder.build_conditional_branch(more, body_bb, done_bb).unwrap();
        self.builder.position_at_end(body_bb);
        let eptr = unsafe {
            self.builder
                .build_gep(buf_arr, buf_ptr, &[i64_ty.const_zero(), idx], "cdtor.elem")
                .unwrap()
        };
        self.emit_field_destroy_for_ty(eptr, elem_ty, 0);
        let next = self.builder.build_int_add(idx, i64_ty.const_int(1, false), "cdtor.next").unwrap();
        self.builder.build_store(idx_ptr, next).unwrap();
        self.builder.build_unconditional_branch(cond_bb).unwrap();
        self.builder.position_at_end(done_bb);
        self.builder.build_return(None).unwrap();
        self.cur_fn = prev_fn;
        if let Some(bb) = prev_block {
            self.builder.position_at_end(bb);
        }
        key
    }

    /// Generated destructor for a map slot with owned keys/values.
    /// Memoized by key/value types.
    fn container_dtor_for_map(
        &mut self,
        key_ty: BasicTypeEnum<'ctx>,
        val_ty: BasicTypeEnum<'ctx>,
    ) -> String {
        let key = format!("map:{:?}:{:?}", key_ty, val_ty);
        if self.container_dtors.contains_key(&key) {
            return key;
        }
        let name = format!("__container_dtor_{}", self.container_dtors.len());
        let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
        let func = self.module.add_function(&name, self.context.void_type().fn_type(&[ptr_ty.into()], false), None);
        self.container_dtors.insert(key.clone(), func);
        let prev_fn = self.cur_fn;
        let prev_block = self.builder.get_insert_block();
        let entry = self.context.append_basic_block(func, "entry");
        self.builder.position_at_end(entry);
        self.cur_fn = Some(func);
        let slot = func.get_nth_param(0).unwrap().into_pointer_value();
        let map_st = self.map_struct_ty(key_ty, val_ty);
        let i64_ty = self.context.i64_type();
        let len_ptr = self.builder.build_struct_gep(map_st, slot, 2, "cmap.len.ptr").unwrap();
        let len = self.builder.build_load(i64_ty, len_ptr, "cmap.len").unwrap().into_int_value();
        let keys_ptr = self.builder.build_struct_gep(map_st, slot, 0, "cmap.keys").unwrap();
        let vals_ptr = self.builder.build_struct_gep(map_st, slot, 1, "cmap.vals").unwrap();
        let arr_of = |t: BasicTypeEnum<'ctx>| -> inkwell::types::ArrayType<'ctx> {
            match t {
                BasicTypeEnum::IntType(it) => it.array_type(Self::MAP_CAP),
                BasicTypeEnum::FloatType(ft) => ft.array_type(Self::MAP_CAP),
                BasicTypeEnum::PointerType(pt) => pt.array_type(Self::MAP_CAP),
                BasicTypeEnum::StructType(st) => st.array_type(Self::MAP_CAP),
                BasicTypeEnum::ArrayType(at) => at.array_type(Self::MAP_CAP),
                _ => self.context.i64_type().array_type(Self::MAP_CAP),
            }
        };
        let keys_arr = arr_of(key_ty);
        let vals_arr = arr_of(val_ty);
        let idx_ptr = self.create_entry_block_alloca("__cdtor_idx", i64_ty.into());
        self.builder.build_store(idx_ptr, i64_ty.const_zero()).unwrap();
        let cond_bb = self.context.append_basic_block(func, "cdtor.cond");
        let body_bb = self.context.append_basic_block(func, "cdtor.body");
        let done_bb = self.context.append_basic_block(func, "cdtor.done");
        self.builder.build_unconditional_branch(cond_bb).unwrap();
        self.builder.position_at_end(cond_bb);
        let idx = self.builder.build_load(i64_ty, idx_ptr, "cdtor.idx").unwrap().into_int_value();
        let more = self.builder.build_int_compare(IntPredicate::SLT, idx, len, "cdtor.more").unwrap();
        self.builder.build_conditional_branch(more, body_bb, done_bb).unwrap();
        self.builder.position_at_end(body_bb);
        let kptr = unsafe {
            self.builder
                .build_gep(keys_arr, keys_ptr, &[i64_ty.const_zero(), idx], "cdtor.key")
                .unwrap()
        };
        self.emit_field_destroy_for_ty(kptr, key_ty, 0);
        let vptr = unsafe {
            self.builder
                .build_gep(vals_arr, vals_ptr, &[i64_ty.const_zero(), idx], "cdtor.val")
                .unwrap()
        };
        self.emit_field_destroy_for_ty(vptr, val_ty, 0);
        let next = self.builder.build_int_add(idx, i64_ty.const_int(1, false), "cdtor.next").unwrap();
        self.builder.build_store(idx_ptr, next).unwrap();
        self.builder.build_unconditional_branch(cond_bb).unwrap();
        self.builder.position_at_end(done_bb);
        self.builder.build_return(None).unwrap();
        self.cur_fn = prev_fn;
        if let Some(bb) = prev_block {
            self.builder.position_at_end(bb);
        }
        format!("map:{:?}:{:?}", key_ty, val_ty)
    }

    /// Whether this struct/class needs structural destruction: an `own`
    /// field anywhere inside (nested structs and fixed arrays included;
    /// vectors/maps are gated to Own-P2b). Cycle-safe via the pair/type
    /// walk.
    fn struct_needs_field_destroy(&self, name: &str) -> bool {
        let lookup = name.rsplit("::").next().unwrap_or(name);
        match self.struct_types.get(lookup) {
            Some(st) => self.type_has_own_pair(&st.as_basic_type_enum(), &mut HashSet::new()),
            None => false,
        }
    }

    /// Destroy the `own` fields of the struct at `struct_ptr` (reverse
    /// declaration order), recursing into nested structs and fixed arrays.
    fn emit_struct_field_destroy(&mut self, struct_ptr: PointerValue<'ctx>, struct_name: &str) {
        let lookup = struct_name.rsplit("::").next().unwrap_or(struct_name);
        let (st, fmap) = match (self.struct_types.get(lookup), self.struct_fields.get(lookup)) {
            (Some(st), Some(fm)) => (*st, fm.clone()),
            _ => return,
        };
        let mut fields: Vec<(String, u32)> = fmap.into_iter().collect();
        fields.sort_by_key(|(_, idx)| *idx);
        for (_, idx) in fields.iter().rev() {
            let fty = st.get_field_type_at_index(*idx).unwrap();
            let field_ptr = self
                .builder
                .build_struct_gep(st, struct_ptr, *idx, "field.dtor")
                .unwrap();
            self.emit_field_destroy_for_ty(field_ptr, fty, 0);
        }
    }

    /// Destroy owned content at `ptr` of LLVM type `ty`: `own` pairs via
    /// `emit_own_destroy`, structs field-wise, fixed arrays element-wise.
    /// `depth` guards recursive types.
    fn emit_field_destroy_for_ty(
        &mut self,
        ptr: PointerValue<'ctx>,
        ty: BasicTypeEnum<'ctx>,
        depth: u32,
    ) {
        if depth > 64 {
            return;
        }
        match ty {
            BasicTypeEnum::StructType(st) => {
                if let Some(inner) = self.pair_owner_of(st) {
                    self.emit_own_destroy(ptr, &inner);
                    return;
                }
                let count = st.count_fields();
                for idx in (0..count).rev() {
                    let fty = st.get_field_type_at_index(idx).unwrap();
                    // Skip leaves fast: only descend into aggregates that
                    // could own (pairs/structs/arrays).
                    let maybe_own = matches!(
                        fty,
                        BasicTypeEnum::StructType(_) | BasicTypeEnum::ArrayType(_)
                    );
                    if !maybe_own {
                        continue;
                    }
                    let field_ptr = self
                        .builder
                        .build_struct_gep(st, ptr, idx, "field.dtor")
                        .unwrap();
                    self.emit_field_destroy_for_ty(field_ptr, fty, depth + 1);
                }
            }
            BasicTypeEnum::ArrayType(at) => {
                let elem = at.get_element_type();
                let elem_maybe_own = matches!(
                    elem,
                    BasicTypeEnum::StructType(_) | BasicTypeEnum::ArrayType(_)
                );
                if !elem_maybe_own {
                    return;
                }
                let ctx: &'ctx Context = self.context;
                let i64_ty = ctx.i64_type();
                for i in 0..at.len() {
                    let elem_ptr = unsafe {
                        self.builder
                            .build_gep(at, ptr, &[i64_ty.const_zero(), i64_ty.const_int(i as u64, false)], "arr.elem.dtor")
                            .unwrap()
                    };
                    self.emit_field_destroy_for_ty(elem_ptr, elem, depth + 1);
                }
            }
            _ => {}
        }
    }

    /// Null (poison) the `own` content at `ptr` of LLVM type `ty`, mirroring
    /// `emit_field_destroy_for_ty`: direct pairs become null pairs, struct
    /// and fixed-array interiors recurse. Unconditionally safe: slots sema
    /// poisoned never read again.
    fn null_own_fields(&mut self, ptr: PointerValue<'ctx>, ty: BasicTypeEnum<'ctx>, depth: u32) {
        if depth > 64 {
            return;
        }
        match ty {
            BasicTypeEnum::StructType(st) => {
                if self.pair_owner_of(st).is_some() {
                    self.builder.build_store(ptr, ty.const_zero()).unwrap();
                    return;
                }
                let count = st.count_fields();
                for idx in 0..count {
                    let fty = st.get_field_type_at_index(idx).unwrap();
                    let maybe_own = matches!(
                        fty,
                        BasicTypeEnum::StructType(_) | BasicTypeEnum::ArrayType(_)
                    );
                    if !maybe_own {
                        continue;
                    }
                    let field_ptr = self
                        .builder
                        .build_struct_gep(st, ptr, idx, "field.poison")
                        .unwrap();
                    self.null_own_fields(field_ptr, fty, depth + 1);
                }
            }
            BasicTypeEnum::ArrayType(at) => {
                let elem = at.get_element_type();
                let elem_maybe_own = matches!(
                    elem,
                    BasicTypeEnum::StructType(_) | BasicTypeEnum::ArrayType(_)
                );
                if !elem_maybe_own {
                    return;
                }
                let ctx: &'ctx Context = self.context;
                let i64_ty = ctx.i64_type();
                for i in 0..at.len() {
                    let elem_ptr = unsafe {
                        self.builder
                            .build_gep(at, ptr, &[i64_ty.const_zero(), i64_ty.const_int(i as u64, false)], "arr.elem.poison")
                            .unwrap()
                    };
                    self.null_own_fields(elem_ptr, elem, depth + 1);
                }
            }
            _ => {}
        }
    }

    /// Destroy an `own` slot: load the `{data, tag}` pair, guard on
    /// non-null data, dispatch destructor (static for concrete, switch
    /// for trait), then `free` and poison the slot.
    fn emit_own_destroy(&mut self, pair_slot: PointerValue<'ctx>, inner: &str) {
        let pair_ty = match self.pair_types.get(inner) {
            Some(ty) => *ty,
            None => return,
        };
        let pair_val = self.builder.build_load(pair_ty.as_basic_type_enum(), pair_slot, "own.load").unwrap();
        let data = self.builder.build_extract_value(pair_val.into_struct_value(), 0, "own.data").unwrap().into_pointer_value();
        let func = match self.cur_fn { Some(f) => f, None => return };
        let cur_bb = self.builder.get_insert_block().unwrap();
        let done_bb = self.context.append_basic_block(func, "own.done");
        let is_null = self.builder.build_is_null(data, "own.is_null").unwrap();
        let work_bb = self.context.append_basic_block(func, "own.work");
        self.builder.build_conditional_branch(is_null, done_bb, work_bb).unwrap();
        self.builder.position_at_end(work_bb);
        let is_trait = self.trait_names.contains(inner);
        if is_trait {
            let tag = self.builder.build_extract_value(pair_val.into_struct_value(), 1, "own.tag").unwrap().into_int_value();
            let default_bb = self.context.append_basic_block(func, "own.dtor.default");
            let mut cases: Vec<(inkwell::values::IntValue<'ctx>, inkwell::basic_block::BasicBlock<'ctx>)> = Vec::new();
            for cls in self.implementors_of(inner) {
                let Some(dtors) = self.class_destructors.get(&cls).cloned() else { continue };
                if dtors.is_empty() { continue; }
                let Some(tag_const) = self.class_tags.get(&cls).cloned() else { continue };
                let arm_bb = self.context.append_basic_block(func, "own.dtor.arm");
                cases.push((self.context.i64_type().const_int(tag_const, false), arm_bb));
                self.builder.position_at_end(arm_bb);
                for (dtor_fn, _) in &dtors {
                    let _ = self.builder.build_call(*dtor_fn, &[data.into()], "own.dtor.call");
                }
                let free = self.get_or_declare_free();
                self.builder.build_call(free, &[data.into()], "own.free.arm").unwrap();
                self.builder.build_unconditional_branch(done_bb).unwrap();
            }
            self.builder.position_at_end(default_bb);
            let free = self.get_or_declare_free();
            self.builder.build_call(free, &[data.into()], "own.free.default").unwrap();
            self.builder.build_unconditional_branch(done_bb).unwrap();
            self.builder.position_at_end(work_bb);
            if cases.is_empty() {
                let free = self.get_or_declare_free();
                self.builder.build_call(free, &[data.into()], "own.free").unwrap();
                self.builder.build_unconditional_branch(done_bb).unwrap();
            } else {
                self.builder.build_switch(tag, default_bb, &cases).unwrap();
            }
        } else {
            if let Some(dtors) = self.class_destructors.get(inner).cloned() {
                for (dtor_fn, _) in &dtors {
                    let _ = self.builder.build_call(*dtor_fn, &[data.into()], "own.dtor.call");
                }
            }
            let free = self.get_or_declare_free();
            self.builder.build_call(free, &[data.into()], "own.free").unwrap();
            self.builder.build_unconditional_branch(done_bb).unwrap();
        }
        self.builder.position_at_end(done_bb);
        self.builder.build_store(pair_slot, pair_ty.const_zero()).unwrap();
    }
    fn emit_current_scope_owns(&mut self) {
        if let Some(slots) = self.own_slots.last().cloned() {
            for (ptr, inner) in slots.iter().rev() {
                self.emit_own_destroy(*ptr, inner);
            }
        }
    }
    fn emit_all_owns(&mut self) {
        let owned = self.own_slots.clone();
        for scope in owned.iter().rev() {
            for (ptr, inner) in scope.iter().rev() {
                self.emit_own_destroy(*ptr, inner);
            }
        }
    }

    /// Null every `own` slot named in value-forwarding position within `expr`
    /// (a move into an `own` slot transfers ownership out of each of them).
    /// Mirrors `sema::Checker::moved_ident_names`: transparent through
    /// parentheses, conditional branches, and match arm bodies; stops at
    /// calls, member access, and closures. Nulling is unconditional, hence
    /// leak-leaning for untaken branches (sema poisoned every named source,
    /// so none of them can be read afterwards) — but the taken branch never
    /// double-frees. `except` skips one variable (self-assignment guard).
    fn null_moved_sources(&mut self, expr: &Expr, except: Option<&str>) {
        let mut names = Vec::new();
        Self::collect_moved_names(expr, &mut names);
        for name in names {
            if Some(name.as_str()) == except {
                continue;
            }
            let lookup = name.rsplit("::").next().unwrap_or(&name);
            if let Some((ptr, ty)) = self.lookup_var(&name).or_else(|| self.lookup_var(lookup)) {
                if ty.is_struct_type() {
                    let st = ty.into_struct_type();
                    if self.pair_owner_of(st).is_some() {
                        self.builder.build_store(ptr, ty.const_zero()).unwrap();
                    } else if self.type_has_own_pair(&ty, &mut HashSet::new()) {
                        // Struct with transitive `own` fields: poison the
                        // owned interior (mirrors sema poisoning).
                        self.null_own_fields(ptr, ty, 0);
                    }
                }
            }
        }
    }

    /// Identifier names in transparent value-forwarding positions: the
    /// expression itself, parentheses, conditional branches, and match arm
    /// bodies. Must stay in sync with `sema::Checker::moved_ident_names`.
    fn collect_moved_names(expr: &Expr, out: &mut Vec<String>) {
        match &expr.kind {
            ExprKind::Ident(n) => out.push(n.clone()),
            ExprKind::Paren(e) => Self::collect_moved_names(e, out),
            ExprKind::Conditional {
                then_branch,
                else_branch,
                ..
            } => {
                Self::collect_moved_names(then_branch, out);
                Self::collect_moved_names(else_branch, out);
            }
            ExprKind::Match(m) => {
                for arm in &m.arms {
                    if let MatchArmBody::Expr(e) = &arm.body {
                        Self::collect_moved_names(e, out);
                    }
                }
            }
            _ => {}
        }
    }

    /// Track an `own` parameter in the current (outermost) own-scope so an
    /// early `return` (`emit_all_owns`) and the fall-through exit destroy it.
    /// Callers must have pushed an outer scope first; `ref`/`out` params must
    /// not be tracked (caller-owned). Mirrors the free-function prologue.
    fn track_own_param(&mut self, alloca: PointerValue<'ctx>, ty: &Type) {
        if let Type::Own(inner, _) = ty {
            let inner_name = match inner.as_ref() {
                Type::Named(n, _) => n.rsplit("::").next().unwrap_or(n).to_string(),
                Type::Generic(n, _, _) => n.rsplit("::").next().unwrap_or(n).to_string(),
                _ => String::new(),
            };
            if !inner_name.is_empty() {
                if let Some(top) = self.own_slots.last_mut() {
                    top.push((alloca, inner_name));
                }
            }
        }
    }

    /// Destroy a trait-typed slot: load the `{data, tag}` pair and switch
    /// over implementors that declare destructors. Implementors without
    /// one take the (safe, empty) default branch.
    fn emit_trait_dtor_call(&mut self, pair_slot: PointerValue<'ctx>, tname: &str) {
        let Some(pair_ty) = self.pair_types.get(tname).cloned() else {
            return;
        };
        let Some(func) = self.cur_fn else {
            return;
        };
        let pair = self
            .builder
            .build_load(pair_ty.as_basic_type_enum(), pair_slot, "trait.dtor.pair")
            .unwrap();
        let data = self
            .builder
            .build_extract_value(pair.into_struct_value(), 0, "trait.dtor.data")
            .unwrap()
            .into_pointer_value();
        let tag = self
            .builder
            .build_extract_value(pair.into_struct_value(), 1, "trait.dtor.tag")
            .unwrap()
            .into_int_value();
        let cur_bb = self.builder.get_insert_block().unwrap();
        let merge_bb = self.context.append_basic_block(func, "trait.dtor.merge");
        let default_bb = self
            .context
            .append_basic_block(func, "trait.dtor.skip");
        let mut cases = Vec::new();
        for cls in self.implementors_of(tname) {
            let Some(dtors) = self.class_destructors.get(&cls).cloned() else {
                continue;
            };
            let Some(tag_const) = self.class_tags.get(&cls).cloned() else {
                continue;
            };
            let arm_bb = self.context.append_basic_block(func, "trait.dtor.arm");
            cases.push((
                self.context.i64_type().const_int(tag_const, false),
                arm_bb,
            ));
            self.builder.position_at_end(arm_bb);
            for (dtor_fn, _) in &dtors {
                let _ = self.builder.build_call(
                    *dtor_fn,
                    &[data.into()],
                    "trait.dtor.call",
                );
            }
            self.builder.build_unconditional_branch(merge_bb).unwrap();
        }
        self.builder.position_at_end(default_bb);
        self.builder.build_unconditional_branch(merge_bb).unwrap();
        self.builder.position_at_end(cur_bb);
        self.builder.build_switch(tag, default_bb, &cases).unwrap();
        self.builder.position_at_end(merge_bb);
    }

    /// Emit destructor calls for the innermost scope (reverse declaration order).
    fn emit_current_scope_dtors(&mut self) {
        if let Some(scope) = self.scope_dtors.last().cloned() {
            for (alloca, class_name) in scope.iter().rev() {
                self.emit_dtor_call(*alloca, class_name);
            }
        }
    }

    /// Emit destructor calls for all active scopes (innermost first).
    fn emit_all_dtors(&mut self) {
        let scopes = self.scope_dtors.clone();
        for scope in scopes.iter().rev() {
            for (alloca, class_name) in scope.iter().rev() {
                self.emit_dtor_call(*alloca, class_name);
            }
        }
    }

    /// Emit destructor calls for scopes at or above `target_depth`
    /// (mirrors `emit_defers_up_to`; `target_depth` is the preserved prefix).
    fn emit_dtors_up_to(&mut self, target_depth: usize) {
        let scopes = self.scope_dtors.clone();
        for scope in scopes.iter().skip(target_depth).rev() {
            for (alloca, class_name) in scope.iter().rev() {
                self.emit_dtor_call(*alloca, class_name);
            }
        }
    }

    /// Program startup for `main`: evaluate pending complex global
    /// initializers in declaration order first (const-folding only covers
    /// literals), then run the user `init` block if present (previously
    /// emitted but never called) so it observes initialized globals.
    fn emit_program_startup(&mut self) -> Result<(), CodegenError> {
        let pendings = std::mem::take(&mut self.pending_global_inits);
        for (name, init) in &pendings {
            if let Some((slot, dest_ty)) = self.globals.get(name).cloned() {
                let v = self.codegen_expr(init)?;
                let v = self.box_trait_value(v, dest_ty, init.span)?;
                let v = self.coerce_to_ty(v, dest_ty);
                self.builder.build_store(slot, v).unwrap();
                self.null_moved_sources(init, None);
            }
        }
        if let Some(init_fn) = self.module.get_function("hella.init") {
            self.builder.build_call(init_fn, &[], "hella.init.call").unwrap();
        }
        Ok(())
    }

    /// Destroy globals owning heap data at program end (`main` exit only):
    /// `own` pairs plus user/struct destructors, reverse declaration order.
    /// Array entries expand per static index.
    fn emit_global_dtors(&mut self) {
        for (ptr, inner) in self.global_owns.clone().iter().rev() {
            self.emit_own_destroy(*ptr, inner);
        }
        for entry in self.global_dtors.clone().iter().rev() {
            match entry {
                GlobalDtor::One(ptr, name) => self.emit_dtor_call(*ptr, name),
                GlobalDtor::Array { slot, elem_ty, len } => {
                    let ctx: &'ctx Context = self.context;
                    let i64_ty = ctx.i64_type();
                    let buf_ty = elem_ty.array_type(*len);
                    for i in (0..*len).rev() {
                        let eptr = unsafe {
                            self.builder
                                .build_gep(buf_ty, *slot, &[i64_ty.const_zero(), i64_ty.const_int(i as u64, false)], "arr.elem.dtor")
                                .unwrap()
                        };
                        self.emit_field_destroy_for_ty(eptr, *elem_ty, 0);
                    }
                }
            }
        }
    }

    fn codegen_property(&mut self, class: &ClassDecl, prop: &PropertyDecl) -> Result<(), CodegenError> {
        if let Some(getter) = &prop.getter {
            let mangled = format!("{}__get_{}", class.name, prop.name);
            let func = self.module.get_function(&mangled).ok_or(CodegenError{message: format!("getter not declared {}", mangled), span: prop.span})?;
            self.cur_fn = Some(func);
            self.cur_class = Some(class.name.clone());
            let entry = self.context.append_basic_block(func, "entry");
            self.builder.position_at_end(entry);
            self.vars.push(HashMap::new());
            let this_ty: BasicTypeEnum<'ctx> = self.context.ptr_type(inkwell::AddressSpace::default()).into();
            let this_param = func.get_nth_param(0).unwrap();
            let this_alloca = self.create_entry_block_alloca("this", this_ty);
            self.builder.build_store(this_alloca, this_param).unwrap();
            self.vars.last_mut().unwrap().insert("this".to_string(), (this_alloca, this_ty));
            let always_returns = self.codegen_block(getter)?;
            if !always_returns && self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                // getter must return something; emit zero
                if let Some(prop_ty) = prop.ty.as_ref() {
                    let ty = self.llvm_ty_for(prop_ty);
                    let zero = match prop_ty {
                        Type::Int(_) => self.context.i64_type().const_int(0,false).into(),
                        Type::Bool(_) => self.context.bool_type().const_int(0,false).into(),
                        _ => self.context.i64_type().const_int(0,false).into(),
                    };
                    // try to handle typed getter
                    let llvm_ty = self.llvm_ty_for(prop_ty);
                    // build return of zero of that type if possible
                    let ret_val = if llvm_ty.is_int_type() { self.context.i64_type().const_int(0,false).as_basic_value_enum() } else { zero };
                    self.builder.build_return(Some(&ret_val)).unwrap();
                } else {
                    self.builder.build_return(None).unwrap();
                }
            }
            self.vars.pop();
            self.cur_fn = None;
            self.cur_class = None;
            if !func.verify(true) { return Err(CodegenError{message: format!("getter {} failed verify", mangled), span: prop.span}); }
        }
        if let Some((param, body)) = &prop.setter {
            let mangled = format!("{}__set_{}", class.name, prop.name);
            let func = self.module.get_function(&mangled).ok_or(CodegenError{message: format!("setter not declared {}", mangled), span: prop.span})?;
            self.cur_fn = Some(func);
            self.cur_class = Some(class.name.clone());
            let entry = self.context.append_basic_block(func, "entry");
            self.builder.position_at_end(entry);
            self.vars.push(HashMap::new());
            let this_ty: BasicTypeEnum<'ctx> = self.context.ptr_type(inkwell::AddressSpace::default()).into();
            let this_param = func.get_nth_param(0).unwrap();
            let this_alloca = self.create_entry_block_alloca("this", this_ty);
            self.builder.build_store(this_alloca, this_param).unwrap();
            self.vars.last_mut().unwrap().insert("this".to_string(), (this_alloca, this_ty));
            let llvm_ty = self.llvm_ty_for(&param.ty);
            let alloca = self.create_entry_block_alloca(&param.name, llvm_ty);
            let val = func.get_nth_param(1).unwrap();
            self.builder.build_store(alloca, val).unwrap();
            self.vars.last_mut().unwrap().insert(param.name.clone(), (alloca, llvm_ty));
                        if matches!(&param.ty, Type::Vec { .. }) { self.vec_vars.insert(param.name.clone()); }
                        if matches!(&param.ty, Type::Map { .. }) { self.map_vars.insert(param.name.clone()); }
                        if matches!(&param.ty, Type::String(_)) { self.string_vars.insert(param.name.clone()); }
                        self.track_unsigned_var(&param.name, &param.ty);
            let _ = self.codegen_block(body)?;
            if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                self.builder.build_return(None).unwrap();
            }
            self.vars.pop();
            self.cur_fn = None;
            self.cur_class = None;
            if !func.verify(true) { return Err(CodegenError{message: format!("setter {} failed verify", mangled), span: prop.span}); }
        }
        Ok(())
    }

    fn codegen_operator(&mut self, class: &ClassDecl, op: &OperatorDecl) -> Result<(), CodegenError> {
        let op_map = self.class_operators.get(&class.name).ok_or(CodegenError{message: format!("operator not declared for {}", class.name), span: op.span})?;
        let (func, _) = op_map.get(&op.op).cloned().ok_or(CodegenError{message: format!("operator {} not found", op.op), span: op.span})?;
        self.cur_fn = Some(func);
        self.cur_class = Some(class.name.clone());
        let entry = self.context.append_basic_block(func, "entry");
        self.builder.position_at_end(entry);
        self.vars.push(std::collections::HashMap::new());
        self.own_slots.push(Vec::new());
        self.scope_dtors.push(Vec::new());
        let this_ty: BasicTypeEnum<'ctx> = self.context.ptr_type(inkwell::AddressSpace::default()).into();
        let this_param = func.get_nth_param(0).unwrap();
        let this_alloca = self.create_entry_block_alloca("this", this_ty);
        self.builder.build_store(this_alloca, this_param).unwrap();
        self.vars.last_mut().unwrap().insert("this".to_string(), (this_alloca, this_ty));
        for (i, param) in op.params.iter().enumerate() {
            let llvm_ty = self.llvm_ty_for(&param.ty);
            let val = func.get_nth_param((i+1) as u32).unwrap();
            if param.mode != ParamMode::None {
                // `ref`/`out`: the caller passed a pointer; use it directly.
                let inner_ty = self.llvm_ty_for(&param.ty);
                let ptr = val.into_pointer_value();
                self.vars.last_mut().unwrap().insert(param.name.clone(), (ptr, inner_ty));
            } else {
                let alloca = self.create_entry_block_alloca(&param.name, llvm_ty);
                self.builder.build_store(alloca, val).unwrap();
                self.vars.last_mut().unwrap().insert(param.name.clone(), (alloca, llvm_ty));
                self.track_own_param(alloca, &param.ty);
                self.track_dtor_slot(alloca, &param.ty, llvm_ty);
            }
                        if matches!(&param.ty, Type::Vec { .. }) { self.vec_vars.insert(param.name.clone()); }
                        if matches!(&param.ty, Type::Map { .. }) { self.map_vars.insert(param.name.clone()); }
                        if matches!(&param.ty, Type::String(_)) { self.string_vars.insert(param.name.clone()); }
                        self.track_unsigned_var(&param.name, &param.ty);
        }
        let _ = self.codegen_block(&op.body)?;
        if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
            self.emit_current_scope_owns();
            self.emit_current_scope_dtors();
            self.builder.build_return(Some(&self.context.i64_type().const_int(0,false))).unwrap();
        }
        self.own_slots.pop();
        self.scope_dtors.pop();
        self.vars.pop();
        self.cur_fn = None;
        self.cur_class = None;
        if !func.verify(true) { return Err(CodegenError{message: format!("operator {} failed verify", op.op), span: op.span}); }
        Ok(())
    }

    fn codegen_conversion(&mut self, class: &ClassDecl, conv: &ConversionDecl) -> Result<(), CodegenError> {
        let mangled = format!("{}__conv_{}_to_{}", class.name, conv.from_ty.name().replace("<","_").replace(">","_").replace(",","_"), conv.to_ty.name().replace("<","_").replace(">","_").replace(",","_"));
        // Type the function by its declared target type (previously a hardcoded `i64` stub).
        let to_sema: crate::sema::Ty = (&conv.to_ty).into();
        let to_resolved = self.resolve_ty_for_codegen(&to_sema);
        let ret_llvm: BasicTypeEnum<'ctx> = self.llvm_ty_for_sema(&to_resolved).unwrap_or_else(|| self.context.i64_type().into());
        let func = self.module.get_function(&mangled).unwrap_or_else(|| {
            let fn_ty = ret_llvm.fn_type(&[self.context.ptr_type(inkwell::AddressSpace::default()).into()], false);
            self.module.add_function(&mangled, fn_ty, None)
        });
        self.cur_fn = Some(func);
        self.cur_class = Some(class.name.clone());
        let entry = self.context.append_basic_block(func, "entry");
        self.builder.position_at_end(entry);
        self.vars.push(std::collections::HashMap::new());
        self.own_slots.push(Vec::new());
        let this_ty: BasicTypeEnum<'ctx> = self.context.ptr_type(inkwell::AddressSpace::default()).into();
        let this_param = func.get_nth_param(0).unwrap();
        let this_alloca = self.create_entry_block_alloca("this", this_ty);
        self.builder.build_store(this_alloca, this_param).unwrap();
        self.vars.last_mut().unwrap().insert("this".to_string(), (this_alloca, this_ty));
        let _ = self.codegen_block(&conv.body)?;
        if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
            self.emit_current_scope_owns();
            // `void` targets have no LLVM value type (`llvm_ty_for_sema`
            // yields `None` → `ret_llvm` fell back to `i64`); return zero.
            match self.default_return_value(&to_resolved) {
                Some(zero) => {
                    let cz = self.coerce_to_ty(zero, ret_llvm);
                    self.builder.build_return(Some(&cz)).unwrap();
                }
                None => {
                    let z: BasicValueEnum<'ctx> = self.context.i64_type().const_zero().into();
                    let cz = self.coerce_to_ty(z, ret_llvm);
                    self.builder.build_return(Some(&cz)).unwrap();
                }
            }
        }
        self.own_slots.pop();
        self.vars.pop();
        self.cur_fn = None;
        self.cur_class = None;
        Ok(())
    }

    fn create_entry_block_alloca(
        &self,
        name: &str,
        ty: BasicTypeEnum<'ctx>,
    ) -> PointerValue<'ctx> {
        let func = self.cur_fn.unwrap();
        let entry = func.get_first_basic_block().unwrap();
        let builder = self.context.create_builder();
        if let Some(first) = entry.get_first_instruction() {
            builder.position_before(&first);
        } else {
            builder.position_at_end(entry);
        }
        match ty {
            BasicTypeEnum::IntType(t) => builder.build_alloca(t, name).unwrap(),
            BasicTypeEnum::FloatType(t) => {
                builder.build_alloca(t, name).unwrap()
            }
            BasicTypeEnum::PointerType(t) => {
                builder.build_alloca(t, name).unwrap()
            }
            BasicTypeEnum::ArrayType(t) => {
                builder.build_alloca(t, name).unwrap()
            }
            BasicTypeEnum::StructType(t) => {
                builder.build_alloca(t, name).unwrap()
            }
            BasicTypeEnum::VectorType(t) => {
                builder.build_alloca(t, name).unwrap()
            }
            BasicTypeEnum::ScalableVectorType(t) => {
                builder.build_alloca(t, name).unwrap()
            }
        }
    }

    fn emit_current_scope_defers(&mut self) -> Result<(), CodegenError> {
        if let Some(idx) = self.defer_stack.len().checked_sub(1) {
            let defers = self.defer_stack[idx].clone();
            for defer in defers.iter().rev() {
                match &defer.inner {
                    DeferInner::Expr(e) => { let _ = self.codegen_expr(e)?; }
                    DeferInner::Block(b) => { let _ = self.codegen_block(b)?; }
                }
                if self.builder.get_insert_block().unwrap().get_terminator().is_some() { break; }
            }
        }
        Ok(())
    }
    fn emit_all_defers(&mut self) -> Result<(), CodegenError> {
        for idx in (0..self.defer_stack.len()).rev() {
            let defers = self.defer_stack[idx].clone();
            for defer in defers.iter().rev() {
                match &defer.inner {
                    DeferInner::Expr(e) => { let _ = self.codegen_expr(e)?; }
                    DeferInner::Block(b) => { let _ = self.codegen_block(b)?; }
                }
                if self.builder.get_insert_block().unwrap().get_terminator().is_some() { break; }
            }
        }
        Ok(())
    }
    fn emit_defers_up_to(&mut self, target_depth: usize) -> Result<(), CodegenError> {
        for idx in (target_depth..self.defer_stack.len()).rev() {
            let defers = self.defer_stack[idx].clone();
            for defer in defers.iter().rev() {
                match &defer.inner {
                    DeferInner::Expr(e) => { let _ = self.codegen_expr(e)?; }
                    DeferInner::Block(b) => { let _ = self.codegen_block(b)?; }
                }
                if self.builder.get_insert_block().unwrap().get_terminator().is_some() { break; }
            }
        }
        Ok(())
    }

    fn codegen_block(&mut self, block: &Block) -> Result<bool, CodegenError> {
        self.vars.push(HashMap::new());
        self.defer_stack.push(Vec::new());
        self.scope_dtors.push(Vec::new());
        self.own_slots.push(Vec::new());
        let mut always_returns = false;
        for stmt in &block.stmts {
            if self
                .builder
                .get_insert_block()
                .unwrap()
                .get_terminator()
                .is_some()
            {
                let dead = self
                    .context
                    .append_basic_block(self.cur_fn.unwrap(), "dead");
                self.builder.position_at_end(dead);
            }
            let stmt_returns = self.codegen_stmt(stmt)?;
            if stmt_returns {
                always_returns = true;
            }
        }
        // Emit defers for this block on normal exit, then destructors.
        // Defers run first so deferred code can still use live locals.
        if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
            self.emit_current_scope_defers()?;
            if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                self.emit_current_scope_dtors();
            }
            if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                self.emit_current_scope_owns();
            }
        } else {
            // already terminated, just clear any remaining defers for this scope (they were emitted via return/break)
            if let Some(v) = self.defer_stack.last_mut() { v.clear(); }
            if let Some(v) = self.own_slots.last_mut() { v.clear(); }
        }
        self.own_slots.pop();
        self.scope_dtors.pop();
        self.defer_stack.pop();
        self.vars.pop();
        Ok(always_returns)
    }

    fn codegen_stmt(&mut self, stmt: &Stmt) -> Result<bool, CodegenError> {
        match stmt {
            Stmt::VarDecl(d) => {
                // Fixed arrays with an array-literal initializer allocate the
                // exact length: inferred `int arr x = [...]` uses the init
                // length; explicit `int arr[N] x = [...]` uses N (sema has
                // already verified N == len).
                // Vectors allocate `{ buffer, len }`; `any xs = vec[]` uses
                // i64 slots until `push` establishes the element type.
                let ty = match (&d.ty, &d.init) {
                    (
                        Type::FixedArray { elem, size, .. },
                        Some(init),
                    ) if matches!(init.kind, ExprKind::ArrayLit(_)) => {
                        let n = size.unwrap_or_else(|| match &init.kind {
                            ExprKind::ArrayLit(elems) => elems.len() as u64,
                            _ => 16,
                        }) as u32;
                        let inner = self.llvm_ty_for(elem);
                        match inner {
                            BasicTypeEnum::IntType(it) => it.array_type(n).into(),
                            BasicTypeEnum::PointerType(pt) => pt.array_type(n).into(),
                            BasicTypeEnum::FloatType(ft) => ft.array_type(n).into(),
                            BasicTypeEnum::StructType(st) => st.array_type(n).into(),
                            BasicTypeEnum::ArrayType(at) => at.array_type(n).into(),
                            _ => self.context.i64_type().array_type(n).into(),
                        }
                    }
                    (Type::Any(_), Some(init))
                        if matches!(init.kind, ExprKind::VecEmpty(_)) =>
                    {
                        self.vec_struct_ty(self.context.i64_type().into()).into()
                    }
                    (Type::Map { .. }, _) => {
                        let entries: &[(Expr, Expr)] = match &d.init {
                            Some(init) if matches!(init.kind, ExprKind::MapLit { .. }) => match &init.kind {
                                ExprKind::MapLit { entries, .. } => entries,
                                _ => unreachable!(),
                            },
                            _ => &[],
                        };
                        let (k, v) = self.map_keyval_llvm_ty(&d.ty, entries);
                        self.map_struct_ty(k, v).into()
                    }
                    (Type::Any(_), Some(init))
                        if matches!(init.kind, ExprKind::MapLit { .. }) =>
                    {
                        let entries: &[(Expr, Expr)] = match &init.kind {
                            ExprKind::MapLit { entries, .. } => entries,
                            _ => &[],
                        };
                        let (k, v) = self.map_keyval_llvm_ty(&d.ty, entries);
                        self.map_struct_ty(k, v).into()
                    }
                    _ => self.llvm_ty_for(&d.ty),
                };
                let alloca = self.create_entry_block_alloca(&d.name, ty);
                self.vars
                    .last_mut()
                    .unwrap()
                    .insert(d.name.clone(), (alloca, ty));
                // Track vectors/maps for `push`/index/`for` lowering.
                if matches!(&d.ty, Type::Vec { .. })
                    || matches!(&d.ty, Type::Any(_))
                        && d.init.as_ref().is_some_and(|i| matches!(i.kind, ExprKind::VecEmpty(_)))
                {
                    self.vec_vars.insert(d.name.clone());
                }
                // Track maps for index/`for` lowering.
                if matches!(&d.ty, Type::Map { .. })
                    || matches!(&d.ty, Type::Any(_))
                        && d.init.as_ref().is_some_and(|i| matches!(i.kind, ExprKind::MapLit { .. }))
                {
                    self.map_vars.insert(d.name.clone());
                }
                // Track strings for `len()`/`is_empty()` lowering.
                if matches!(&d.ty, Type::String(_)) {
                    self.string_vars.insert(d.name.clone());
                }
                self.track_unsigned_var(&d.name, &d.ty);
                // Track `task<T>` handles for `await` result typing (Async-6).
                if let Type::Task(el, _) = &d.ty {
                    self.task_vars.insert(d.name.clone(), crate::sema::Ty::Task(Box::new((el.as_ref()).into())));
                }
                // Track class locals with destructors for RAII scope-exit calls.
                // `this` is the borrowed receiver, never owned: skip it so a
                // method/dtor body never destroys its own receiver.
                if d.name != "this" {
                    self.track_dtor_slot(alloca, &d.ty, ty);
                    // Vectors/maps with owned element types: register for
                    // container-dtor invocation at scope exit.
                    self.track_container_dtor(alloca, &d.ty, ty);
                    // Track `own` slots for heap destruction (pair + free).
                    if let Type::Own(inner, _) = &d.ty {
                        let inner_name = match inner.as_ref() {
                            Type::Named(n, _) => n.rsplit("::").next().unwrap_or(n).to_string(),
                            Type::Generic(n, _, _) => n.rsplit("::").next().unwrap_or(n).to_string(),
                            _ => String::new(),
                        };
                        if !inner_name.is_empty() {
                            if let Some(top) = self.own_slots.last_mut() {
                                top.push((alloca, inner_name));
                            }
                        }
                    }
                }
                if let Some(init) = &d.init {
                    // Fixed-array initializer: store each element via GEP so
                    // element widths coerce exactly (e.g. i64 literals into
                    // an `i32 arr` slot).
                    if let (
                        Type::FixedArray { elem, .. },
                        ExprKind::ArrayLit(elems),
                    ) = (&d.ty, &init.kind)
                    {
                        let dest_elem_ty = self.llvm_ty_for(elem);
                        let arr_ty = match ty {
                            BasicTypeEnum::ArrayType(at) => at,
                            _ => unreachable!("fixed-array alloca must be array type"),
                        };
                        let zero = self.context.i64_type().const_int(0, false);
                        for (i, e) in elems.iter().enumerate() {
                            let v = self.codegen_expr(e)?;
                            let cv = self.coerce_to_ty(v, dest_elem_ty);
                            let idx = self.context.i64_type().const_int(i as u64, false);
                            let eptr = unsafe {
                                self.builder
                                    .build_gep(arr_ty, alloca, &[zero, idx], &format!("arr.init.{i}"))
                                    .unwrap()
                            };
                            self.builder.build_store(eptr, cv).unwrap();
                        }
                    } else if let (
                        Type::Vec { .. },
                        ExprKind::ArrayLit(elems),
                    ) = (&d.ty, &init.kind)
                    {
                        // Vector initializer: fill buffer, set length.
                        let dest_elem_ty = self.vec_elem_llvm_ty(&d.ty);
                        let vec_st = match ty {
                            BasicTypeEnum::StructType(st) => st,
                            _ => unreachable!("vector alloca must be struct type"),
                        };
                        let buf_ptr = self.builder.build_struct_gep(vec_st, alloca, 0, "vec.buf").unwrap();
                        let buf_arr_ty = match dest_elem_ty {
                            BasicTypeEnum::IntType(it) => it.array_type(Self::VEC_CAP).into(),
                            BasicTypeEnum::FloatType(ft) => ft.array_type(Self::VEC_CAP).into(),
                            BasicTypeEnum::PointerType(pt) => pt.array_type(Self::VEC_CAP).into(),
                            BasicTypeEnum::StructType(st) => st.array_type(Self::VEC_CAP).into(),
                            BasicTypeEnum::ArrayType(at) => at.array_type(Self::VEC_CAP).into(),
                            _ => self.context.i64_type().array_type(Self::VEC_CAP).into(),
                        };
                        let buf_arr_ty = match buf_arr_ty {
                            BasicTypeEnum::ArrayType(at) => at,
                            _ => unreachable!(),
                        };
                        let zero = self.context.i64_type().const_int(0, false);
                        for (i, e) in elems.iter().enumerate() {
                            let v = self.codegen_expr(e)?;
                            let cv = self.coerce_to_ty(v, dest_elem_ty);
                            let idx = self.context.i64_type().const_int(i as u64, false);
                            let eptr = unsafe {
                                self.builder
                                    .build_gep(buf_arr_ty, buf_ptr, &[zero, idx], &format!("vec.init.{i}"))
                                    .unwrap()
                            };
                            self.builder.build_store(eptr, cv).unwrap();
                        }
                        let len_ptr = self.builder.build_struct_gep(vec_st, alloca, 1, "vec.len").unwrap();
                        self.builder.build_store(len_ptr, self.context.i64_type().const_int(elems.len() as u64, false)).unwrap();
                    } else if matches!(init.kind, ExprKind::VecEmpty(_)) {
                        // Empty vector: zero buffer, length 0.
                        self.builder.build_store(alloca, ty.const_zero()).unwrap();
                    } else if let ExprKind::MapLit { entries, .. } = &init.kind {
                        // Map literal: keys/values into buffers, set length.
                        // (Sema has validated entry types; `any` declarations
                        // infer slots from the literal shape.)
                        let map_entries: &[(Expr, Expr)] = entries;
                        let (dest_key_ty, dest_val_ty) = self.map_keyval_llvm_ty(&d.ty, map_entries);
                        let map_st = match ty {
                            BasicTypeEnum::StructType(st) => st,
                            _ => unreachable!("map alloca must be struct type"),
                        };
                        self.store_map_entries(alloca, map_st, dest_key_ty, dest_val_ty, map_entries)?;
                    } else {
                        let val = self.codegen_expr(init)?;
                        // Box class values flowing into trait-typed slots.
                        let val = self.box_trait_value(val, ty, d.span)?;
                        let coerced = self.coerce_to_ty(val, ty);
                        self.builder.build_store(alloca, coerced).unwrap();
                        // Move from owned source: null the source slot(s) so their
                        // scope destroy becomes a no-op (poison is checked in sema).
                        // Transparent through `?:`/parens/match arms (see
                        // `null_moved_sources`); untaken branches merely leak.
                        // Fires for `own` decls and decls with `own` fields.
                        let decl_owned = match &d.ty {
                            Type::Own(_, _) => true,
                            Type::Named(n, _) | Type::Generic(n, _, _) => {
                                self.struct_needs_field_destroy(n.rsplit("::").next().unwrap_or(n))
                            }
                            _ => false,
                        };
                        if decl_owned {
                            self.null_moved_sources(init, None);
                        }
                    }
                } else {
                    // zero init for all types
                    let zero: BasicValueEnum = match &d.ty {
                        Type::Int(_) => {
                            self.context.i64_type().const_int(0, false).into()
                        }
                        Type::Bool(_) => {
                            self.context.bool_type().const_int(0, false).into()
                        }
                        Type::Char(_) => {
                            self.context.i32_type().const_int(0, false).into()
                        }
                        Type::String(_) => self
                            .context
                            .ptr_type(inkwell::AddressSpace::default())
                            .const_null()
                            .into(),
                        Type::Float(_) => self.context.f32_type().const_float(0.0).into(),
                        Type::Double(_) => self.context.f64_type().const_float(0.0).into(),
                        Type::Void(_) => unreachable!(),
                        Type::Named(n, _) => {
                            let lookup = n.rsplit("::").next().unwrap_or(n);
                            if let Some(st) = self.struct_types.get(lookup) {
                                st.const_zero().into()
                            } else if let Some(et) = self.enum_types.get(lookup) {
                                et.const_zero().into()
                            } else if n.len()==1 && n.chars().next().map(|c| c.is_ascii_uppercase()).unwrap_or(false) {
                                self.context.i64_type().const_int(0,false).into()
                            } else {
                                let st = self.struct_types.get(n).unwrap();
                                st.const_zero().into()
                            }
                        }
                        Type::Own(inner, _) => {
                            // Null pair (sema poisons uninitialized `own`
                            // slots, so this is never observably read).
                            let lookup = match inner.as_ref() {
                                Type::Named(n, _) => {
                                    n.rsplit("::").next().unwrap_or(n).to_string()
                                }
                                _ => String::new(),
                            };
                            match self.pair_types.get(&lookup) {
                                Some(pair) => pair.const_zero().into(),
                                None => self
                                    .context
                                    .ptr_type(inkwell::AddressSpace::default())
                                    .const_null()
                                    .into(),
                            }
                        }
                        Type::Generic(_, _, _) => self.context.ptr_type(inkwell::AddressSpace::default()).const_null().into(),
                        Type::FunctionType(_, _, _) => self.context.ptr_type(inkwell::AddressSpace::default()).const_null().into(),
                        Type::Tuple(_, _) => self.context.ptr_type(inkwell::AddressSpace::default()).const_null().into(),
                        Type::Any(_) => self.context.ptr_type(inkwell::AddressSpace::default()).const_null().into(),
                        Type::Array(_, _) => self
                            .context
                            .i64_type()
                            .array_type(16)
                            .const_zero()
                            .into(),
                        Type::FixedArray { elem, size, .. } => {
                            let n = size.unwrap_or(16) as u32;
                            let inner = self.llvm_ty_for(elem);
                            match inner {
                                BasicTypeEnum::IntType(it) => it.array_type(n).const_zero().into(),
                                BasicTypeEnum::PointerType(pt) => pt.array_type(n).const_zero().into(),
                                BasicTypeEnum::FloatType(ft) => ft.array_type(n).const_zero().into(),
                                BasicTypeEnum::StructType(st) => st.array_type(n).const_zero().into(),
                                BasicTypeEnum::ArrayType(at) => at.array_type(n).const_zero().into(),
                                _ => self.context.i64_type().array_type(n).const_zero().into(),
                            }
                        }
                        Type::Vec { .. } => {
                            let elem = self.vec_elem_llvm_ty(&d.ty);
                            self.vec_struct_ty(elem).const_zero().into()
                        }
                        Type::Map { .. } => {
                            let (k, v) = self.map_keyval_llvm_ty(&d.ty, &[]);
                            self.map_struct_ty(k, v).const_zero().into()
                        }
                        Type::Pointer(_, _) => self
                            .context
                            .ptr_type(inkwell::AddressSpace::default())
                            .const_null()
                            .into(),
                        // `task<T>` (Async-6): null handle (sema rejects
                        // un-initialized task reads via the escape check).
                        Type::Task(_, _) => self
                            .context
                            .ptr_type(inkwell::AddressSpace::default())
                            .const_null()
                            .into(),
                        Type::Optional(el, _) => {
                            let inner_zero: BasicValueEnum = match el.as_ref() {
                                Type::Int(_) => self
                                    .context
                                    .i64_type()
                                    .const_int(0, false)
                                    .into(),
                                _ => self
                                    .context
                                    .i64_type()
                                    .const_int(0, false)
                                    .into(),
                            };
                            // optional as {inner, false}
                            let inner_ty = self.llvm_ty_for(el);
                            let opt_ty = self.context.struct_type(
                                &[
                                    inner_ty.into(),
                                    self.context.bool_type().into(),
                                ],
                                false,
                            );
                            opt_ty
                                .const_named_struct(&[
                                    inner_zero.into(),
                                    self.context
                                        .bool_type()
                                        .const_int(0, false)
                                        .into(),
                                ])
                                .into()
                        }
                    };
                    self.builder.build_store(alloca, zero).unwrap();
                }
                Ok(false)
            }
            Stmt::Const(c) => {
                // Map consts allocate the map struct: explicit `K:V` uses its
                // slots; untyped `const m = has ... end` infers from entries.
                let is_map_const = matches!(c.ty, Some(Type::Map { .. }))
                    || matches!(&c.ty, None) && matches!(c.init.kind, ExprKind::MapLit { .. });
                if is_map_const {
                    let entries: &[(Expr, Expr)] = match &c.init.kind {
                        ExprKind::MapLit { entries, .. } => entries,
                        _ => &[],
                    };
                    let decl_ty: Type = c.ty.clone().unwrap_or(Type::Any(Span::new(0, 0)));
                    let (dk, dv) = self.map_keyval_llvm_ty(&decl_ty, entries);
                    let map_st = self.map_struct_ty(dk, dv);
                    let ty: BasicTypeEnum<'ctx> = map_st.into();
                    let alloca = self.create_entry_block_alloca(&c.name, ty);
                    self.vars.last_mut().unwrap().insert(c.name.clone(), (alloca, ty));
                    self.map_vars.insert(c.name.clone());
                    self.store_map_entries(alloca, map_st, dk, dv, entries)?;
                    return Ok(false);
                }
                let ty = if let Some(t) = &c.ty {
                    self.llvm_ty_for(t)
                } else if matches!(c.init.kind, ExprKind::VecEmpty(_)) {
                    // `const xs = vec[]`: undetermined i64-slot vector.
                    self.vec_struct_ty(self.context.i64_type().into()).into()
                } else {
                    // infer from init via sema type? For MVP, assume int
                    // Try to infer by codegen init first to get type, then alloca
                    // Simplify: assume int for now, will be corrected after init codegen
                    self.context.i64_type().into()
                };
                // If ty was inferred as int placeholder but init is string, we need correct ty
                // For `const x = "hello"` with no type, ty should be string (ptr)
                // We can codegen init first to get its type, then create alloca with that type if ty was None
                let is_vec_const = matches!(c.ty, Some(Type::Vec { .. }))
                    || matches!(&c.ty, None) && matches!(c.init.kind, ExprKind::VecEmpty(_));
                if is_vec_const {
                    self.vec_vars.insert(c.name.clone());
                }
                if matches!(c.ty, Some(Type::String(_))) {
                    self.string_vars.insert(c.name.clone());
                }
                if c.ty.as_ref().is_some_and(Self::ast_ty_is_unsigned) {
                    self.unsigned_vars.insert(c.name.clone());
                }
                // Vector const with literal initializer: per-element buffer fill.
                if let (Some(Type::Vec { .. }), ExprKind::ArrayLit(elems)) =
                    (&c.ty, &c.init.kind)
                {
                    let alloca = self.create_entry_block_alloca(&c.name, ty);
                    self.vars.last_mut().unwrap().insert(c.name.clone(), (alloca, ty));
                    let dest_elem_ty = self.vec_elem_llvm_ty(c.ty.as_ref().unwrap());
                    let vec_st = match ty {
                        BasicTypeEnum::StructType(st) => st,
                        _ => unreachable!("vector alloca must be struct type"),
                    };
                    let buf_ptr = self.builder.build_struct_gep(vec_st, alloca, 0, "vec.buf").unwrap();
                    let buf_arr_ty = match dest_elem_ty {
                        BasicTypeEnum::IntType(it) => it.array_type(Self::VEC_CAP).into(),
                        BasicTypeEnum::PointerType(pt) => pt.array_type(Self::VEC_CAP).into(),
                        _ => self.context.i64_type().array_type(Self::VEC_CAP).into(),
                    };
                    let buf_arr_ty = match buf_arr_ty {
                        BasicTypeEnum::ArrayType(at) => at,
                        _ => unreachable!(),
                    };
                    let zero = self.context.i64_type().const_int(0, false);
                    for (i, e) in elems.iter().enumerate() {
                        let v = self.codegen_expr(e)?;
                        let cv = self.coerce_to_ty(v, dest_elem_ty);
                        let idx = self.context.i64_type().const_int(i as u64, false);
                        let eptr = unsafe {
                            self.builder
                                .build_gep(buf_arr_ty, buf_ptr, &[zero, idx], &format!("vec.init.{i}"))
                                .unwrap()
                        };
                        self.builder.build_store(eptr, cv).unwrap();
                    }
                    let len_ptr = self.builder.build_struct_gep(vec_st, alloca, 1, "vec.len").unwrap();
                    self.builder.build_store(len_ptr, self.context.i64_type().const_int(elems.len() as u64, false)).unwrap();
                    return Ok(false);
                }
                let init_val = self.codegen_expr(&c.init)?;
                let actual_ty = if c.ty.is_none() {
                    init_val.get_type()
                } else {
                    ty
                };
                let alloca = self.create_entry_block_alloca(&c.name, actual_ty);
                self.vars.last_mut().unwrap().insert(c.name.clone(), (alloca, actual_ty));
                let unsigned_dest = c.ty.as_ref().is_some_and(Self::ast_ty_is_unsigned)
                    || self.is_unsigned_expr(&c.init);
                let stored = self.coerce_to_ty_with_unsigned(init_val, actual_ty, unsigned_dest);
                self.builder.build_store(alloca, stored).unwrap();
                Ok(false)
            }
            Stmt::Destructure(d) => {
                let val = self.codegen_expr(&d.expr)?;
                let val_ty = val.get_type();
                for (idx, target) in d.targets.iter().enumerate() {
                    match target {
                        DestructureTarget::Wildcard(_) => {},
                        DestructureTarget::Ident(name, _) => {
                            // Extract element at idx from tuple/array value
                            let elem_val = if val_ty.is_struct_type() {
                                self.builder.build_extract_value(val.into_struct_value(), idx as u32, &format!("destructure.{}", name)).unwrap()
                            } else if val_ty.is_array_type() {
                                self.builder.build_extract_value(val.into_array_value(), idx as u32, &format!("destructure.{}", name)).unwrap()
                            } else {
                                // fallback: if val is not struct/array, try to extract as struct (tuple)
                                // For array stored as [16 x i64], extract_value works as above
                                // If val is pointer (should not happen), load?
                                val
                            };
                            // Check if var already exists (assign) or new (decl)
                            if let Some((ptr, _)) = self.lookup_var(name) {
                                self.builder.build_store(ptr, elem_val).unwrap();
                            } else {
                                let alloca = self.create_entry_block_alloca(name, elem_val.get_type());
                                self.builder.build_store(alloca, elem_val).unwrap();
                                self.vars.last_mut().unwrap().insert(name.clone(), (alloca, elem_val.get_type()));
                            }
                        }
                    }
                }
                Ok(false)
            }
            Stmt::Assert(a) => {
                // `debug_assert` compiles to nothing in release builds
                // (Rust/C-style; sema still checked the condition).
                if a.is_debug && self.release {
                    return Ok(false);
                }
                let cond_val = self.codegen_expr(&a.cond)?.into_int_value();
                let cur_fn = self.cur_fn.unwrap();
                let assert_ok = self.context.append_basic_block(cur_fn, "assert.ok");
                let assert_fail = self.context.append_basic_block(cur_fn, "assert.fail");
                self.builder.build_conditional_branch(cond_val, assert_ok, assert_fail).unwrap();
                self.builder.position_at_end(assert_fail);
                // print message if provided
                if let Some(msg) = &a.message {
                    let msg_val = self.codegen_expr(msg)?;
                    if msg_val.is_pointer_value() {
                        let puts = self.get_or_declare_puts();
                        self.builder.build_call(puts, &[msg_val.into()], "puts_assert").unwrap();
                    } else {
                        // for non-string message, try to print as int?
                        let fmt = self.builder.build_global_string_ptr("assertion failed: %lld\n", "assert_fmt").unwrap();
                        let printf = self.get_or_declare_printf();
                        self.builder.build_call(printf, &[fmt.as_pointer_value().into(), msg_val.into()], "printf_assert").unwrap();
                    }
                } else {
                    let default_msg = self.builder.build_global_string_ptr("assertion failed", "assert_default").unwrap();
                    let puts = self.get_or_declare_puts();
                    self.builder.build_call(puts, &[default_msg.as_pointer_value().into()], "puts_assert_default").unwrap();
                }
                let abort = self.get_or_declare_abort();
                self.builder.build_call(abort, &[], "abort").unwrap();
                self.builder.build_unreachable().unwrap();
                self.builder.position_at_end(assert_ok);
                Ok(false)
            }
            Stmt::Expr(e) => {
                let _ = self.codegen_expr(&e.expr)?;
                Ok(false)
            }
            Stmt::Block(b) => self.codegen_block(b),
            // `scope do ... end` (Async-6): at codegen a scope is a plain
            // block — the join is structural (sema forced every task to be
            // awaited inside), so no runtime barrier is emitted here.
            Stmt::Scope(b) => self.codegen_block(b),
            // `yield` (Async-6): cooperative checkpoint. Thread-backed
            // tasks yield the OS thread (`sched_yield`); the declaration is
            // lazy so sync programs never reference it (Async-8).
            Stmt::Yield(span) => {
                let f = self.get_or_declare_sched_yield();
                self.builder.build_call(f, &[], "async.yield").unwrap();
                let _ = span;
                Ok(false)
            }
            Stmt::Return(r) => {
                self.emit_all_defers()?;
                // Evaluate the return operand BEFORE destructors run. The
                // operand may move out of locals (nulling below makes their
                // destroys no-ops); destroying first would free the very
                // object being returned. (Defers intentionally stay first:
                // they observe live state, as before.)
                let evaluated: Option<BasicValueEnum<'ctx>> = match &r.value {
                    Some(expr) => Some(self.codegen_expr(expr)?),
                    None => None,
                };
                // Move from owned source: null the source slot(s) so their
                // scope destroy becomes a no-op (sema poisoned them).
                // Transparent through `?:`/parens/match arms. Fires for
                // `own` returns and returns with `own` fields.
                if let Some(expr) = &r.value {
                    if let Some(cur) = self.cur_fn {
                        if let Some(ret_ty) = cur.get_type().get_return_type() {
                            if ret_ty.is_struct_type() {
                                let ret_st = ret_ty.into_struct_type();
                                let ret_owned = self.pair_owner_of(ret_st).is_some()
                                    || self.ty_to_struct_name(&ret_ty).map(|n| self.struct_needs_field_destroy(&n)).unwrap_or(false);
                                if ret_owned {
                                    self.null_moved_sources(expr, None);
                                }
                            }
                        }
                    }
                }
                if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                    self.emit_all_dtors();
                }
                if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                    self.emit_all_owns();
                }
                if self.cur_is_main {
                    // Program end: destroy owning globals (reverse declared).
                    self.emit_global_dtors();
                    if let Some(val) = evaluated {
                        let ret_val = if val.is_int_value()
                            && val.into_int_value().get_type().get_bit_width()
                                == 64
                        {
                            self.builder
                                .build_int_truncate(
                                    val.into_int_value(),
                                    self.context.i32_type(),
                                    "main.trunc",
                                )
                                .unwrap()
                                .into()
                        } else if val.is_int_value()
                            && val.into_int_value().get_type().get_bit_width()
                                == 1
                        {
                            self.builder
                                .build_int_z_extend(
                                    val.into_int_value(),
                                    self.context.i32_type(),
                                    "main.zext",
                                )
                                .unwrap()
                                .into()
                        } else if val.is_struct_value() {
                            // struct main? not supported, return 0
                            self.context.i32_type().const_int(0, false).into()
                        } else {
                            val
                        };
                        self.builder.build_return(Some(&ret_val)).unwrap();
                    } else {
                        let zero = self.context.i32_type().const_int(0, false);
                        self.builder.build_return(Some(&zero)).unwrap();
                    }
                } else if let Some(val) = evaluated {
                    // Coerce int return to the function's declared return width
                    // (e.g. `i32 foo() do return 5 end` — literal is i64).
                    // Class values returning through a trait-typed slot are
                    // boxed into `{data, tag}` pairs first.
                    let mut val = val;
                    if let Some(cur) = self.cur_fn {
                        if let Some(ret_ty) = cur.get_type().get_return_type() {
                            val = self.box_trait_value(val, ret_ty, r.span)?;
                        }
                    }
                    let coerced = if val.is_int_value() {
                        if let Some(cur) = self.cur_fn {
                            if let Some(ret_ty) = cur.get_type().get_return_type() {
                                self.coerce_to_ty(val, ret_ty)
                            } else {
                                val
                            }
                        } else {
                            val
                        }
                    } else {
                        val
                    };
                    self.builder.build_return(Some(&coerced)).unwrap();
                } else {
                    self.builder.build_return(None).unwrap();
                }
                Ok(true)
            }
            Stmt::If(s) => {
                let cond = self.codegen_expr(&s.cond)?;
                let cond_bool = cond.into_int_value();
                let func = self.cur_fn.unwrap();
                let then_bb = self.context.append_basic_block(func, "if.then");
                let else_bb = if s.else_block.is_some() {
                    Some(self.context.append_basic_block(func, "if.else"))
                } else {
                    None
                };
                let merge_bb =
                    self.context.append_basic_block(func, "if.merge");
                if let Some(else_bb) = else_bb {
                    self.builder
                        .build_conditional_branch(cond_bool, then_bb, else_bb)
                        .unwrap();
                } else {
                    self.builder
                        .build_conditional_branch(cond_bool, then_bb, merge_bb)
                        .unwrap();
                }
                self.builder.position_at_end(then_bb);
                let then_ret = self.codegen_block(&s.then_block)?;
                if self
                    .builder
                    .get_insert_block()
                    .unwrap()
                    .get_terminator()
                    .is_none()
                {
                    self.builder.build_unconditional_branch(merge_bb).unwrap();
                }
                let else_ret = if let (Some(else_bb), Some(else_block)) =
                    (else_bb, &s.else_block)
                {
                    self.builder.position_at_end(else_bb);
                    let r = self.codegen_block(else_block)?;
                    if self
                        .builder
                        .get_insert_block()
                        .unwrap()
                        .get_terminator()
                        .is_none()
                    {
                        self.builder
                            .build_unconditional_branch(merge_bb)
                            .unwrap();
                    }
                    r
                } else {
                    false
                };
                self.builder.position_at_end(merge_bb);
                Ok(then_ret && else_ret)
            }
            Stmt::While(s) => {
                let func = self.cur_fn.unwrap();
                let cond_bb = self.context.append_basic_block(func, "while.cond");
                let body_bb = self.context.append_basic_block(func, "while.body");
                let exit_bb = self.context.append_basic_block(func, "while.exit");
                self.builder.build_unconditional_branch(cond_bb).unwrap();
                self.builder.position_at_end(cond_bb);
                let cond = self.codegen_expr(&s.cond)?;
                let cond_bool = cond.into_int_value();
                self.builder.build_conditional_branch(cond_bool, body_bb, exit_bb).unwrap();
                self.loop_stack.push(LoopContext{cond_bb, exit_bb, label: None, defer_depth: self.defer_stack.len()});
                self.builder.position_at_end(body_bb);
                let _ = self.codegen_block(&s.body)?;
                if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                    self.builder.build_unconditional_branch(cond_bb).unwrap();
                }
                self.loop_stack.pop();
                self.builder.position_at_end(exit_bb);
                Ok(false)
            }
            Stmt::Loop(l) => {
                let func = self.cur_fn.unwrap();
                let header_bb = self.context.append_basic_block(func, "loop.header");
                let body_bb = self.context.append_basic_block(func, "loop.body");
                let exit_bb = self.context.append_basic_block(func, "loop.exit");
                self.builder.build_unconditional_branch(header_bb).unwrap();
                self.builder.position_at_end(header_bb);
                self.builder.build_unconditional_branch(body_bb).unwrap();
                self.loop_stack.push(LoopContext{cond_bb: header_bb, exit_bb, label: l.label.clone(), defer_depth: self.defer_stack.len()});
                self.builder.position_at_end(body_bb);
                let _ = self.codegen_block(&l.body)?;
                if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                    self.builder.build_unconditional_branch(header_bb).unwrap();
                }
                self.loop_stack.pop();
                self.builder.position_at_end(exit_bb);
                Ok(false)
            }
            Stmt::For(f) => {
                // Desugar for var in iter do body => index loop over array
                // iter must be array (int[]); element type is int
                let func = self.cur_fn.unwrap();
                // Resolve the iterable once. `Ident` slots are used directly;
                // any other expression is evaluated once into a temp slot so
                // array literals, calls, etc. iterate correctly. Anything that
                // is not an array, vector, map, or string is a codegen error
                // (sema rejects it first; this is the backstop, replacing the
                // old silent len-16/var-0 fallback that miscompiled valid
                // non-`Ident` iters).
                enum ForIterKind { Array, Vec, Map, Str, Range }
                let (iter_ptr, iter_ty, iter_kind): (PointerValue<'ctx>, BasicTypeEnum<'ctx>, ForIterKind) = if let ExprKind::Ident(ref arr_name) = f.iter.kind {
                    let (p, t) = self.lookup_var(arr_name).ok_or(CodegenError{message: format!("undefined variable `{arr_name}`"), span: f.iter.span})?;
                    if t.is_array_type() {
                        (p, t, ForIterKind::Array)
                    } else if t.is_struct_type() && self.is_vec_var(arr_name) {
                        (p, t, ForIterKind::Vec)
                    } else if t.is_struct_type() && self.is_map_var(arr_name) {
                        (p, t, ForIterKind::Map)
                    } else if t.is_struct_type() && Self::is_range_struct(t) {
                        (p, t, ForIterKind::Range)
                    } else if t.is_pointer_type() || self.is_string_var(arr_name) {
                        (p, t, ForIterKind::Str)
                    } else {
                        return Err(CodegenError{message: "`for` iterable must be array, vector, map, string, or range".into(), span: f.iter.span});
                    }
                } else {
                    let v = self.codegen_expr(&f.iter)?;
                    let vt: BasicTypeEnum<'ctx> = v.get_type();
                    let tmp = self.create_entry_block_alloca(&format!("__for_iter_{}", f.var), vt);
                    self.builder.build_store(tmp, v).unwrap();
                    if vt.is_array_type() {
                        (tmp, vt, ForIterKind::Array)
                    } else if vt.is_pointer_type() {
                        (tmp, vt, ForIterKind::Str)
                    } else if vt.is_struct_type() {
                        // Vec is `{buf, len}` (2 fields), map is
                        // `{keys, vals, len}` (3 fields) — but only when the
                        // first field is a buffer array. A range value
                        // `{i64 start, i64 end, i1 inclusive}` also has 3
                        // fields and must iterate lazily, never as a map
                        // (that miscompile hung forever on garbage bounds).
                        let st = vt.into_struct_type();
                        let first_is_buf = matches!(
                            st.get_field_type_at_index(0).unwrap(),
                            BasicTypeEnum::ArrayType(_)
                        );
                        match st.count_fields() {
                            3 if first_is_buf => (tmp, vt, ForIterKind::Map),
                            3 if Self::is_range_struct(vt) => (tmp, vt, ForIterKind::Range),
                            2 if first_is_buf => (tmp, vt, ForIterKind::Vec),
                            _ => return Err(CodegenError{message: "`for` iterable must be array, vector, map, string, or range".into(), span: f.iter.span}),
                        }
                    } else {
                        return Err(CodegenError{message: "`for` iterable must be array, vector, map, or string".into(), span: f.iter.span});
                    }
                };
                let cond_bb = self.context.append_basic_block(func, "for.cond");
                let body_bb = self.context.append_basic_block(func, "for.body");
                let inc_bb = self.context.append_basic_block(func, "for.inc");
                let exit_bb = self.context.append_basic_block(func, "for.exit");
                // Allocate index var __for_idx_<var>
                let idx_name = format!("__for_idx_{}", f.var);
                let idx_ty = self.context.i64_type().as_basic_type_enum();
                let idx_ptr = self.create_entry_block_alloca(&idx_name, idx_ty);
                self.builder.build_store(idx_ptr, self.context.i64_type().const_int(0, false)).unwrap();
                // Determine array to iterate: resolved above (direct slot for
                // `Ident`, temp slot otherwise).
                // Array length comes from the actual LLVM array type (fixed
                // `arr[N]` uses N; legacy `T[]` uses 16). Vectors iterate to
                // their loaded length. Maps iterate over their keys.
                let iter_is_vec = matches!(iter_kind, ForIterKind::Vec);
                let iter_is_map = matches!(iter_kind, ForIterKind::Map);
                let iter_len_const: Option<u64> = match iter_kind {
                    ForIterKind::Array => Some(iter_ty.into_array_type().len() as u64),
                    _ => None,
                };
                // Strings iterate to their loaded length (strlen), not a
                // hardcoded size.
                let iter_is_str_here = matches!(iter_kind, ForIterKind::Str);
                // Create initial branch to cond
                self.builder.build_unconditional_branch(cond_bb).unwrap();
                self.builder.position_at_end(cond_bb);
                let idx_val = self.builder.build_load(idx_ty, idx_ptr, "for.idx.load").unwrap().into_int_value();
                // Vectors iterate to their loaded length; arrays to the const size
                // — except main's `args`, which iterates to argc so `for a in
                // args` skips the null padding. Maps iterate over keys up to
                // the loaded length. Strings iterate to their loaded length
                // (strlen).
                let limit = if iter_is_str_here {
                    let sptr = self.builder.build_load(iter_ty, iter_ptr, "for.str.ptr").unwrap().into_pointer_value();
                    let call = self.builder.build_call(self.get_or_declare_strlen(), &[sptr.into()], "for.str.len").unwrap();
                    call.try_as_basic_value().basic().unwrap().into_int_value()
                } else if iter_is_vec || iter_is_map {
                    let vec_st = iter_ty.into_struct_type();
                    let len_ptr = self.builder.build_struct_gep(vec_st, iter_ptr, if iter_is_map { 2 } else { 1 }, "for.iter.len.ptr").unwrap();
                    self.builder.build_load(self.context.i64_type(), len_ptr, "for.iter.len").unwrap().into_int_value()
                } else if matches!(iter_kind, ForIterKind::Array) {
                    match self.main_args_len(iter_ptr) {
                        Some(dyn_len) => dyn_len,
                        None => self.context.i64_type().const_int(iter_len_const.unwrap_or(16), false),
                    }
                } else if matches!(iter_kind, ForIterKind::Range) {
                    // Lazy range bound: max((inclusive ? end+1 : end) - start, 0).
                    let i64_ty = self.context.i64_type();
                    let rst = iter_ty.into_struct_type();
                    let start_ptr = self.builder.build_struct_gep(rst, iter_ptr, 0, "for.range.start.ptr").unwrap();
                    let end_ptr = self.builder.build_struct_gep(rst, iter_ptr, 1, "for.range.end.ptr").unwrap();
                    let incl_ptr = self.builder.build_struct_gep(rst, iter_ptr, 2, "for.range.incl.ptr").unwrap();
                    let start = self.builder.build_load(i64_ty, start_ptr, "for.range.start").unwrap().into_int_value();
                    let end = self.builder.build_load(i64_ty, end_ptr, "for.range.end").unwrap().into_int_value();
                    let incl = self.builder.build_load(self.context.bool_type(), incl_ptr, "for.range.incl").unwrap().into_int_value();
                    let end_p1 = self.builder.build_int_add(end, i64_ty.const_int(1, false), "for.range.endp1").unwrap();
                    let end_excl = self.builder.build_select(incl, end_p1, end, "for.range.endx").unwrap().into_int_value();
                    let span = self.builder.build_int_sub(end_excl, start, "for.range.span").unwrap();
                    let pos = self.builder.build_int_compare(IntPredicate::SGT, span, i64_ty.const_zero(), "for.range.pos").unwrap();
                    self.builder.build_select(pos, span, i64_ty.const_zero(), "for.range.count").unwrap().into_int_value()
                } else {
                    self.context.i64_type().const_int(iter_len_const.unwrap_or(16), false)
                };
                let cond = self.builder.build_int_compare(IntPredicate::SLT, idx_val, limit, "for.cond").unwrap();
                self.builder.build_conditional_branch(cond, body_bb, exit_bb).unwrap();
                self.loop_stack.push(LoopContext{cond_bb: inc_bb, exit_bb, label: f.label.clone(), defer_depth: self.defer_stack.len()});
                self.builder.position_at_end(body_bb);
                // Load element from the resolved iterable slot.
                // Create loop scope for var
                self.vars.push(HashMap::new());
                self.defer_stack.push(Vec::new());
                self.scope_dtors.push(Vec::new());
                // Declare for var in this scope
                let iter_is_vec_here = iter_is_vec;
                let iter_is_map_here = iter_is_map;
                let arr_ty = iter_ty;
                let arr_ptr = iter_ptr;
                let elem_val: Option<BasicValueEnum<'ctx>> = if arr_ty.is_array_type() {
                        let arr_ty_a = arr_ty.into_array_type();
                        let elem_ptr = unsafe { self.builder.build_gep(arr_ty_a, arr_ptr, &[self.context.i64_type().const_int(0,false), idx_val], "for.elem.ptr").unwrap() };
                        let elem_ty = arr_ty_a.get_element_type();
                        Some(self.builder.build_load(elem_ty, elem_ptr, "for.elem").unwrap())
                    } else if (iter_is_vec_here || iter_is_map_here) && arr_ty.is_struct_type() {
                        // Vector element: buffer is struct field 0.
                        // Map iteration yields keys: keys buffer is field 0.
                        let vec_st = arr_ty.into_struct_type();
                        let buf_ptr = self.builder.build_struct_gep(vec_st, arr_ptr, 0, "for.iter.buf").unwrap();
                        let buf_field_ty = vec_st.get_field_type_at_index(0).unwrap();
                        match buf_field_ty {
                            BasicTypeEnum::ArrayType(buf_arr_ty) => {
                                let elem_ptr = unsafe { self.builder.build_gep(buf_arr_ty, buf_ptr, &[self.context.i64_type().const_int(0,false), idx_val], "for.iter.elem.ptr").unwrap() };
                                let elem_ty = buf_arr_ty.get_element_type();
                                Some(self.builder.build_load(elem_ty, elem_ptr, "for.iter.elem").unwrap())
                            }
                            _ => None,
                        }
                    } else if matches!(iter_kind, ForIterKind::Range) && arr_ty.is_struct_type() {
                        // Range iteration yields `start + idx` (i64); the
                        // bound above already encodes start/end/inclusivity.
                        let rst = arr_ty.into_struct_type();
                        let start_ptr = self.builder.build_struct_gep(rst, arr_ptr, 0, "for.range.vstart.ptr").unwrap();
                        let start = self.builder.build_load(self.context.i64_type(), start_ptr, "for.range.vstart").unwrap().into_int_value();
                        let v = self.builder.build_int_add(start, idx_val, "for.range.var").unwrap();
                        Some(v.into())
                    } else if arr_ty.is_pointer_type() {
                        // Strings: byte-stepped load, zero-extended to `char`
                        // (i32). Other pointers cannot occur per sema.
                        let loaded_arr = self.builder.build_load(arr_ty, arr_ptr, "ptr.load").unwrap().into_pointer_value();
                        let elem_ptr = unsafe { self.builder.build_gep(self.context.i8_type(), loaded_arr, &[idx_val], "for.str.elem").unwrap() };
                        let ch = self.builder.build_load(self.context.i8_type(), elem_ptr, "for.str.ch").unwrap().into_int_value();
                        Some(self.builder.build_int_z_extend(ch, self.context.i32_type(), "for.elem").unwrap().into())
                    } else { None };
                if let Some(v) = elem_val {
                    let elem_ty = v.get_type();
                    let var_ptr = self.create_entry_block_alloca(&f.var, elem_ty);
                    self.builder.build_store(var_ptr, v).unwrap();
                    self.vars.last_mut().unwrap().insert(f.var.clone(), (var_ptr, elem_ty));
                } else {
                    // fallback: declare var as int 0 if we couldn't resolve
                    let var_ptr = self.create_entry_block_alloca(&f.var, self.context.i64_type().into());
                    self.builder.build_store(var_ptr, self.context.i64_type().const_int(0,false)).unwrap();
                    self.vars.last_mut().unwrap().insert(f.var.clone(), (var_ptr, self.context.i64_type().into()));
                }
                // Optional second loop variable: the index for arrays,
                // vectors and strings; the value for maps.
                if let Some((v2, _)) = &f.var2 {
                    if iter_is_map_here {
                        let map_st = iter_ty.into_struct_type();
                        let vals_ptr = self.builder.build_struct_gep(map_st, iter_ptr, 1, "for.map.vals").unwrap();
                        if let BasicTypeEnum::ArrayType(vals_arr_ty) = map_st.get_field_type_at_index(1).unwrap() {
                            let val_elem_ty = vals_arr_ty.get_element_type();
                            let vptr = unsafe {
                                self.builder.build_gep(vals_arr_ty, vals_ptr, &[self.context.i64_type().const_int(0, false), idx_val], "for.map.val.ptr").unwrap()
                            };
                            let vv = self.builder.build_load(val_elem_ty, vptr, "for.map.val").unwrap();
                            let v2_ptr = self.create_entry_block_alloca(v2, val_elem_ty);
                            self.builder.build_store(v2_ptr, vv).unwrap();
                            self.vars.last_mut().unwrap().insert(v2.clone(), (v2_ptr, val_elem_ty));
                        }
                    } else {
                        let i64_ty = self.context.i64_type().into();
                        let v2_ptr = self.create_entry_block_alloca(v2, i64_ty);
                        self.builder.build_store(v2_ptr, idx_val).unwrap();
                        self.vars.last_mut().unwrap().insert(v2.clone(), (v2_ptr, i64_ty));
                    }
                }
                let _ = self.codegen_block(&f.body)?;
                // after body, branch to inc
                if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                    // emit defer for for-body scope before inc? The defer inside for body should run before inc
                    // Our codegen_block for body already emitted its defers on normal exit via its own defer handling
                    // But we still have outer for-var scope defers to emit before inc
                    // For simplicity, just branch to inc; inc will handle idx increment
                }
                // Pop for-var scope defer/var (but keep defer for next iteration? The for-var scope is per-iteration; we need to pop after body)
                // Actually for-var scope should be per iteration, but we pushed it before body; after body we should pop and emit its defers
                // Emit defers for for-var scope
                if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                    self.emit_current_scope_defers().unwrap();
                }
                if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                    self.emit_current_scope_dtors();
                }
                self.scope_dtors.pop();
                self.defer_stack.pop();
                self.vars.pop();
                if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                    self.builder.build_unconditional_branch(inc_bb).unwrap();
                }
                self.builder.position_at_end(inc_bb);
                let cur_idx = self.builder.build_load(idx_ty, idx_ptr, "for.idx").unwrap().into_int_value();
                let inc = self.builder.build_int_add(cur_idx, self.context.i64_type().const_int(1,false), "for.inc").unwrap();
                self.builder.build_store(idx_ptr, inc).unwrap();
                self.builder.build_unconditional_branch(cond_bb).unwrap();
                self.loop_stack.pop();
                self.builder.position_at_end(exit_bb);
                Ok(false)
            }
            Stmt::Defer(d) => {
                // Push onto current defer scope (innermost block)
                if let Some(top) = self.defer_stack.last_mut() {
                    top.push(d.clone());
                } else {
                    // No active block defer stack (should not happen, fallback to global)
                    self.defer_stack.push(vec![d.clone()]);
                }
                Ok(false)
            }
            Stmt::Delete(d) => {
                // `delete` on an `own` slot: immediate destroy + free + poison
                if let ExprKind::Ident(name) = &d.target.kind {
                    let lookup = name.rsplit("::").next().unwrap_or(name);
                    if let Some((ptr, ty)) = self.lookup_var(name).or_else(|| self.lookup_var(lookup)) {
                        if ty.is_struct_type() {
                            let st = ty.into_struct_type();
                            if let Some(inner) = self.pair_owner_of(st) {
                                self.emit_own_destroy(ptr, &inner);
                                return Ok(false);
                            }
                        }
                    }
                }
                // Fallback: evaluate for side effects (sema already diagnosed)
                let _ = self.codegen_expr(&d.target)?;
                Ok(false)
            }
            Stmt::Break(b) => {
                let target_idx = if let Some(label) = &b.label {
                    self.loop_stack.iter().rposition(|lc| lc.label.as_ref() == Some(label))
                        .ok_or(CodegenError{message: format!("break label `{label}` not found"), span: b.span})?
                } else {
                    self.loop_stack.len().checked_sub(1).ok_or(CodegenError{message: "break outside loop".into(), span: b.span})?
                };
                let ctx = self.loop_stack[target_idx].clone();
                self.emit_defers_up_to(ctx.defer_depth)?;
                self.emit_dtors_up_to(ctx.defer_depth);
                self.builder.build_unconditional_branch(ctx.exit_bb).unwrap();
                Ok(false)
            }
            Stmt::Continue(c) => {
                let target_idx = if let Some(label) = &c.label {
                    self.loop_stack.iter().rposition(|lc| lc.label.as_ref() == Some(label))
                        .ok_or(CodegenError{message: format!("continue label `{label}` not found"), span: c.span})?
                } else {
                    self.loop_stack.len().checked_sub(1).ok_or(CodegenError{message: "continue outside loop".into(), span: c.span})?
                };
                let ctx = self.loop_stack[target_idx].clone();
                self.emit_defers_up_to(ctx.defer_depth)?;
                self.emit_dtors_up_to(ctx.defer_depth);
                self.builder.build_unconditional_branch(ctx.cond_bb).unwrap();
                Ok(false)
            }
        }
    }

    fn codegen_expr(
        &mut self,
        expr: &Expr,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        match &expr.kind {
            ExprKind::IntLit(v) => {
                Ok(self.context.i64_type().const_int(*v as u64, true).into())
            }
            ExprKind::FloatLit(v) => {
                let f: f64 = v.parse().unwrap_or(0.0);
                Ok(self.context.f64_type().const_float(f).into())
            }
            ExprKind::BoolLit(b) => Ok(self
                .context
                .bool_type()
                .const_int(if *b { 1 } else { 0 }, false)
                .into()),
            ExprKind::CharLit(c) => Ok(self.context.i32_type().const_int(*c as u64, false).into()),
            ExprKind::Ident(name) => {
                let lookup = name.rsplit("::").next().unwrap_or(name);
                if let Some((ptr, ty)) = self.lookup_var(name).or_else(|| self.lookup_var(lookup)) {
                    Ok(self.builder.build_load(ty, ptr, lookup).unwrap())
                } else if let Some((func, _)) = self.funcs.get(name).or_else(|| self.funcs.get(lookup)).cloned() {
                    // First-class function reference: the function's address
                    // (flows into `function<Ret(Args)>` slots, called via the
                    // variable-call path).
                    Ok(func.as_global_value().as_pointer_value().into())
                } else if let Some(f) = self.module.get_function(name).or_else(|| self.module.get_function(lookup)) {
                    // Extern functions live directly on the module.
                    Ok(f.as_global_value().as_pointer_value().into())
                } else {
                    Err(CodegenError {
                        message: format!("undefined var {name}"),
                        span: expr.span,
                    })
                }
            }
            ExprKind::This => {
                let (ptr, ty) = self.lookup_var("this").ok_or(CodegenError{message: "`this` outside method".into(), span: expr.span})?;
                Ok(self.builder.build_load(ty, ptr, "this").unwrap())
            }
            ExprKind::MethodCall{object, method, method_span: _, args} => {
                // Vector `push` (types skill §10): `v.push(x)` appends `x`,
                // growing `len`. Capacity is VEC_CAP; overflow traps via abort.
                if method == "push" {
                    if let ExprKind::Ident(name) = &object.kind {
                        if self.is_vec_var(name) {
                            if args.len() != 1 {
                                return Err(CodegenError{message: format!("`push` expects 1 arg, found {}", args.len()), span: expr.span});
                            }
                            let (ptr, ty) = self.lookup_var(name).ok_or(CodegenError{message: format!("undefined var {name}"), span: object.span})?;
                            let vec_st = match ty {
                                BasicTypeEnum::StructType(st) => st,
                                _ => return Err(CodegenError{message: format!("`push` on non-vector `{name}`"), span: object.span}),
                            };
                            let arg_val = self.codegen_call_arg(&args[0])?;
                            // Buffer element type from the struct layout.
                            let buf_field_ty = vec_st.get_field_type_at_index(0).unwrap();
                            let buf_arr_ty = match buf_field_ty {
                                BasicTypeEnum::ArrayType(at) => at,
                                _ => return Err(CodegenError{message: "`push`: malformed vector buffer".into(), span: object.span}),
                            };
                            let dest_elem_ty = buf_arr_ty.get_element_type();
                            let cv = self.coerce_to_ty(arg_val, dest_elem_ty);
                            // len = vec.len; if len >= CAP abort; buf[len] = v; len += 1
                            let len_ptr = self.builder.build_struct_gep(vec_st, ptr, 1, "vec.len.ptr").unwrap();
                            let len = self.builder.build_load(self.context.i64_type(), len_ptr, "vec.len").unwrap().into_int_value();
                            let cap = self.context.i64_type().const_int(Self::VEC_CAP as u64, false);
                            let ok = self.builder.build_int_compare(IntPredicate::ULT, len, cap, "vec.cap.ok").unwrap();
                            let func = self.cur_fn.ok_or(CodegenError{message: "`push` outside function".into(), span: expr.span})?;
                            let ok_bb = self.context.append_basic_block(func, "vec.push.ok");
                            let fail_bb = self.context.append_basic_block(func, "vec.push.fail");
                            self.builder.build_conditional_branch(ok, ok_bb, fail_bb).unwrap();
                            self.builder.position_at_end(fail_bb);
                            self.builder.build_call(self.get_or_declare_abort(), &[], "vec.push.abort").unwrap();
                            self.builder.build_unreachable().unwrap();
                            self.builder.position_at_end(ok_bb);
                            let buf_ptr = self.builder.build_struct_gep(vec_st, ptr, 0, "vec.buf.ptr").unwrap();
                            let zero = self.context.i64_type().const_int(0, false);
                            let eptr = unsafe {
                                self.builder
                                    .build_gep(buf_arr_ty, buf_ptr, &[zero, len], "vec.push.slot")
                                    .unwrap()
                            };
                            self.builder.build_store(eptr, cv).unwrap();
                            let one = self.context.i64_type().const_int(1, false);
                            let nlen = self.builder.build_int_add(len, one, "vec.len.inc").unwrap();
                            self.builder.build_store(len_ptr, nlen).unwrap();
                            return Ok(self.context.i64_type().const_int(0, false).into());
                        }
                    }
                }
                // Collection and string methods (`len`, `push` aside, `pop`,
                // `contains`, `get`, ...). Sema has validated arity/types.
                if let ExprKind::Ident(name) = &object.kind {
                    if let Some(ret) = self.codegen_collection_method(name, method, args, expr.span)? {
                        return Ok(ret);
                    }
                }
                // Determine this pointer for method call
                let this_ptr: PointerValue<'ctx> = match &object.kind {
                    ExprKind::Ident(name) => {
                        if let Some((ptr, ty)) = self.lookup_var(name) {
                            if ty.is_struct_type() {
                                let st = ty.into_struct_type();
                                if let Some(_inner) = self.pair_owner_of(st) {
                                    let pair_val = self.builder.build_load(ty, ptr, "own.load").unwrap();
                                    self.builder.build_extract_value(pair_val.into_struct_value(), 0, "own.data").unwrap().into_pointer_value()
                                } else {
                                    // p is struct value instance, its alloca is the instance pointer
                                    ptr
                                }
                            } else if ty.is_pointer_type() {
                                self.builder.build_load(ty, ptr, "this.load").unwrap().into_pointer_value()
                            } else {
                                return Err(CodegenError{message: format!("method call on non-class variable `{name}`"), span: expr.span});
                            }
                        } else { return Err(CodegenError{message: format!("undefined var {name}"), span: expr.span}); }
                    }
                    ExprKind::This => {
                        let (ptr, ty) = self.lookup_var("this").ok_or(CodegenError{message: "`this` outside method".into(), span: expr.span})?;
                        self.builder.build_load(ty, ptr, "this.load").unwrap().into_pointer_value()
                    }
                    ExprKind::Super => {
                        // Same object as `this`; dispatch resolves to the
                        // parent implementation via `infer_expr_ty`.
                        let (ptr, ty) = self.lookup_var("this").ok_or(CodegenError{message: "`super` outside method".into(), span: expr.span})?;
                        self.builder.build_load(ty, ptr, "this.load").unwrap().into_pointer_value()
                    }
                    ExprKind::MemberAccess{..} => {
                        // `a.b.method()` where the field may itself be `own`:
                        // resolve the receiver with auto-deref.
                        self.obj_struct_ptr(object)?.0
                    }
                    _ => {
                        // Fallback: try codegen object as value and allocate temp? For now error
                        return Err(CodegenError{message: "method call object must be variable or field access".into(), span: expr.span});
                    }
                };
                let obj_ty = self.infer_expr_ty(object)?;
                // Trait-typed receiver: closed-world dynamic dispatch over
                // the known implementors (see `codegen_trait_method_call`).
                if let crate::sema::Ty::Struct(ref n) = obj_ty {
                    if self.trait_names.contains(n) {
                        return self.codegen_trait_method_call(object, n, method, args, expr.span);
                    }
                }
                let cls_name = match obj_ty {
                    crate::sema::Ty::Struct(ref n) => n.clone(),
                    _ => return Err(CodegenError{message: format!("method call on non-class"), span: expr.span}),
                };
                let methods = self.class_methods.get(&cls_name).ok_or(CodegenError{message: format!("unknown class {cls_name}"), span: expr.span})?;
                let (func, info) = methods.get(method).cloned().ok_or(CodegenError{message: format!("unknown method {method} for class {cls_name}"), span: expr.span})?;
                let arg_vals = self.codegen_method_call_args(args, &info, this_ptr, expr.span)?;
                let call = self.builder.build_call(func, &arg_vals, "call").unwrap();
                let vk = call.try_as_basic_value();
                if vk.is_basic() { Ok(vk.basic().unwrap()) } else { Ok(self.context.i64_type().const_int(0,false).into()) }
            }
            ExprKind::Paren(inner) => self.codegen_expr(inner),
            ExprKind::Unary { op, expr: inner } => {
                let v = self.codegen_expr(inner)?;
                match op {
                    UnaryOp::Not => {
                        let b = v.into_int_value();
                        Ok(self
                            .builder
                            .build_xor(
                                b,
                                self.context.bool_type().const_int(1, false),
                                "not",
                            )
                            .unwrap()
                            .into())
                    }
                    UnaryOp::Neg => {
                        let i = v.into_int_value();
                        Ok(self
                            .builder
                            .build_int_sub(
                                self.context.i64_type().const_int(0, false),
                                i,
                                "neg",
                            )
                            .unwrap()
                            .into())
                    }
                    UnaryOp::Pos => Ok(v),
                    UnaryOp::BitNot => {
                        let i = v.into_int_value();
                        Ok(self.builder.build_not(i, "bitnot").unwrap().into())
                    }
                    UnaryOp::Inc => {
                        // Prefix ++ : increment lvalue and return new value
                        let ptr = self.codegen_as_ptr(inner)?;
                        let cur = self.builder.build_load(self.context.i64_type(), ptr, "inc.load").unwrap().into_int_value();
                        let nxt = self.builder.build_int_add(cur, self.context.i64_type().const_int(1,false), "inc").unwrap();
                        self.builder.build_store(ptr, nxt).unwrap();
                        Ok(nxt.into())
                    }
                    UnaryOp::Dec => {
                        let ptr = self.codegen_as_ptr(inner)?;
                        let cur = self.builder.build_load(self.context.i64_type(), ptr, "dec.load").unwrap().into_int_value();
                        let nxt = self.builder.build_int_sub(cur, self.context.i64_type().const_int(1,false), "dec").unwrap();
                        self.builder.build_store(ptr, nxt).unwrap();
                        Ok(nxt.into())
                    }
                }
            }
            ExprKind::Postfix { op, expr: inner } => {
                let ptr = self.codegen_as_ptr(inner)?;
                let cur = self.builder.build_load(self.context.i64_type(), ptr, "post.load").unwrap().into_int_value();
                let nxt = match op {
                    UnaryOp::Inc => self.builder.build_int_add(cur, self.context.i64_type().const_int(1,false), "post.inc").unwrap(),
                    UnaryOp::Dec => self.builder.build_int_sub(cur, self.context.i64_type().const_int(1,false), "post.dec").unwrap(),
                    _ => cur,
                };
                self.builder.build_store(ptr, nxt).unwrap();
                Ok(cur.into())
            }
            ExprKind::Binary { op, lhs, rhs } => {
                // Check for operator overloading
                if let Ok(crate::sema::Ty::Struct(sname)) = self.infer_expr_ty(lhs) {
                    if let Some(op_map) = self.class_operators.get(&sname).cloned() {
                        let op_str = match op {
                            BinOp::Add => "+",
                            BinOp::Sub => "-",
                            BinOp::Mul => "*",
                            BinOp::Div => "/",
                            BinOp::Mod => "%",
                            BinOp::Lt => "<",
                            BinOp::Le => "<=",
                            BinOp::Gt => ">",
                            BinOp::Ge => ">=",
                            BinOp::Is => "is",
                            BinOp::IsNot => "is not",
                            BinOp::And => "and",
                            BinOp::Or => "or",
                            BinOp::BitAnd => "&",
                            BinOp::BitOr => "|",
                            BinOp::BitXor => "^",
                            BinOp::Shl => "<<",
                            BinOp::Shr => ">>",
                            BinOp::NullCoalesce => "??",
                            BinOp::Range => "..",
                            BinOp::RangeInclusive => "..=",
                            _ => "",
                        };
                        if !op_str.is_empty() {
                            if let Some((func,_)) = op_map.get(op_str) {
                                // operator call: this is left operand pointer, arg is right
                                let this_ptr = match self.codegen_as_ptr(lhs) {
                                    Ok(p) => p,
                                    Err(_) => {
                                        // fallback: if lhs is not addressable, allocate temp
                                        let val = self.codegen_expr(lhs)?;
                                        let tmp = self.builder.build_alloca(val.get_type(), "op.lhs.tmp").unwrap();
                                        self.builder.build_store(tmp, val).unwrap();
                                        tmp
                                    }
                                };
                                let r_val = self.codegen_expr(rhs)?;
                                let call = self.builder.build_call(*func, &[this_ptr.into(), r_val.into()], "op.call").unwrap();
                                if let Some(v) = call.try_as_basic_value().basic() { return Ok(v); } else { return Ok(self.context.i64_type().const_int(0,false).into()); }
                            }
                        }
                    }
                }
                let l = self.codegen_expr(lhs)?;
                let r = self.codegen_expr(rhs)?;
                let either_unsigned = self.is_unsigned_expr(lhs) || self.is_unsigned_expr(rhs);
                let (l, r) = self.unify_int_operands_with_unsigned(l, r, either_unsigned);
                Ok(match op {
                    BinOp::Add => self
                        .builder
                        .build_int_add(
                            l.into_int_value(),
                            r.into_int_value(),
                            "add",
                        )
                        .unwrap()
                        .into(),
                    BinOp::Sub => self
                        .builder
                        .build_int_sub(
                            l.into_int_value(),
                            r.into_int_value(),
                            "sub",
                        )
                        .unwrap()
                        .into(),
                    BinOp::Mul => self
                        .builder
                        .build_int_mul(
                            l.into_int_value(),
                            r.into_int_value(),
                            "mul",
                        )
                        .unwrap()
                        .into(),
                    BinOp::Div => self
                        .builder
                        .build_int_signed_div(
                            l.into_int_value(),
                            r.into_int_value(),
                            "div",
                        )
                        .unwrap()
                        .into(),
                    BinOp::Mod => self
                        .builder
                        .build_int_signed_rem(
                            l.into_int_value(),
                            r.into_int_value(),
                            "mod",
                        )
                        .unwrap()
                        .into(),
                    BinOp::Lt => self
                        .builder
                        .build_int_compare(
                            IntPredicate::SLT,
                            l.into_int_value(),
                            r.into_int_value(),
                            "lt",
                        )
                        .unwrap()
                        .into(),
                    BinOp::Le => self
                        .builder
                        .build_int_compare(
                            IntPredicate::SLE,
                            l.into_int_value(),
                            r.into_int_value(),
                            "le",
                        )
                        .unwrap()
                        .into(),
                    BinOp::Gt => self
                        .builder
                        .build_int_compare(
                            IntPredicate::SGT,
                            l.into_int_value(),
                            r.into_int_value(),
                            "gt",
                        )
                        .unwrap()
                        .into(),
                    BinOp::Ge => self
                        .builder
                        .build_int_compare(
                            IntPredicate::SGE,
                            l.into_int_value(),
                            r.into_int_value(),
                            "ge",
                        )
                        .unwrap()
                        .into(),
                    BinOp::Is => {
                        // `own` pairs lower to `{data ptr, tag}` structs: identity is data-ptr equality.
                        if l.is_struct_value() && r.is_struct_value() {
                            let lptr = self.builder.build_extract_value(l.into_struct_value(), 0, "is.ldata").unwrap().into_pointer_value();
                            let rptr = self.builder.build_extract_value(r.into_struct_value(), 0, "is.rdata").unwrap().into_pointer_value();
                            let li = self.builder.build_ptr_to_int(lptr, self.context.i64_type(), "is.li").unwrap();
                            let ri = self.builder.build_ptr_to_int(rptr, self.context.i64_type(), "is.ri").unwrap();
                            self.builder.build_int_compare(IntPredicate::EQ, li, ri, "is").unwrap().into()
                        } else if l.is_pointer_value() && r.is_pointer_value() {
                            let li = self.builder.build_ptr_to_int(l.into_pointer_value(), self.context.i64_type(), "is.li").unwrap();
                            let ri = self.builder.build_ptr_to_int(r.into_pointer_value(), self.context.i64_type(), "is.ri").unwrap();
                            self.builder.build_int_compare(IntPredicate::EQ, li, ri, "is").unwrap().into()
                        } else if l.is_pointer_value() {
                            let li = self.builder.build_ptr_to_int(l.into_pointer_value(), self.context.i64_type(), "is.li").unwrap();
                            let ri = if r.is_int_value() { r.into_int_value() } else { self.builder.build_ptr_to_int(r.into_pointer_value(), self.context.i64_type(), "is.ri").unwrap() };
                            self.builder.build_int_compare(IntPredicate::EQ, li, ri, "is").unwrap().into()
                        } else if r.is_pointer_value() {
                            let ri = self.builder.build_ptr_to_int(r.into_pointer_value(), self.context.i64_type(), "is.ri").unwrap();
                            let li = if l.is_int_value() { l.into_int_value() } else { self.builder.build_ptr_to_int(l.into_pointer_value(), self.context.i64_type(), "is.li").unwrap() };
                            self.builder.build_int_compare(IntPredicate::EQ, li, ri, "is").unwrap().into()
                        } else {
                            self.builder.build_int_compare(IntPredicate::EQ, l.into_int_value(), r.into_int_value(), "is").unwrap().into()
                        }
                    }
                    BinOp::IsNot => {
                        if l.is_struct_value() && r.is_struct_value() {
                            let lptr = self.builder.build_extract_value(l.into_struct_value(), 0, "isnot.ldata").unwrap().into_pointer_value();
                            let rptr = self.builder.build_extract_value(r.into_struct_value(), 0, "isnot.rdata").unwrap().into_pointer_value();
                            let li = self.builder.build_ptr_to_int(lptr, self.context.i64_type(), "isnot.li").unwrap();
                            let ri = self.builder.build_ptr_to_int(rptr, self.context.i64_type(), "isnot.ri").unwrap();
                            self.builder.build_int_compare(IntPredicate::NE, li, ri, "isnot").unwrap().into()
                        } else if l.is_pointer_value() && r.is_pointer_value() {
                            let li = self.builder.build_ptr_to_int(l.into_pointer_value(), self.context.i64_type(), "isnot.li").unwrap();
                            let ri = self.builder.build_ptr_to_int(r.into_pointer_value(), self.context.i64_type(), "isnot.ri").unwrap();
                            self.builder.build_int_compare(IntPredicate::NE, li, ri, "isnot").unwrap().into()
                        } else if l.is_pointer_value() {
                            let li = self.builder.build_ptr_to_int(l.into_pointer_value(), self.context.i64_type(), "isnot.li").unwrap();
                            let ri = if r.is_int_value() { r.into_int_value() } else { self.builder.build_ptr_to_int(r.into_pointer_value(), self.context.i64_type(), "isnot.ri").unwrap() };
                            self.builder.build_int_compare(IntPredicate::NE, li, ri, "isnot").unwrap().into()
                        } else if r.is_pointer_value() {
                            let ri = self.builder.build_ptr_to_int(r.into_pointer_value(), self.context.i64_type(), "isnot.ri").unwrap();
                            let li = if l.is_int_value() { l.into_int_value() } else { self.builder.build_ptr_to_int(l.into_pointer_value(), self.context.i64_type(), "isnot.li").unwrap() };
                            self.builder.build_int_compare(IntPredicate::NE, li, ri, "isnot").unwrap().into()
                        } else {
                            self.builder.build_int_compare(IntPredicate::NE, l.into_int_value(), r.into_int_value(), "isnot").unwrap().into()
                        }
                    }
                    BinOp::And => self
                        .builder
                        .build_and(
                            l.into_int_value(),
                            r.into_int_value(),
                            "and",
                        )
                        .unwrap()
                        .into(),
                    BinOp::Or => self
                        .builder
                        .build_or(l.into_int_value(), r.into_int_value(), "or")
                        .unwrap()
                        .into(),
                    BinOp::BitAnd => self.builder.build_and(l.into_int_value(), r.into_int_value(), "bitand").unwrap().into(),
                    BinOp::BitOr => self.builder.build_or(l.into_int_value(), r.into_int_value(), "bitor").unwrap().into(),
                    BinOp::BitXor => self.builder.build_xor(l.into_int_value(), r.into_int_value(), "bitxor").unwrap().into(),
                    BinOp::Shl => self.builder.build_left_shift(l.into_int_value(), r.into_int_value(), "shl").unwrap().into(),
                    // `>>` is LOGICAL (zero-fill) for unsigned operands:
                    // `u64`/`uN` (stdlib types skill) are unsigned, and a
                    // sign-propagating shift would corrupt random/PRNG code
                    // whose values legitimately have the high bit set.
                    // Signed int-like types keep the arithmetic shift.
                    BinOp::Shr => {
                        // inkwell's `is_signed` means "emit an arithmetic
                        // (sign-propagating) shift": true for signed int-
                        // likes, false for unsigned (logical, zero-fill).
                        // `u64`/`uN`/`uint` must be logical — PRNG-style
                        // code relies on the high bit being a data bit.
                        // Signedness comes from decl-site tracking: LLVM
                        // ints carry none, so `infer_expr_ty` alone
                        // reports every int local as `Ty::Int`.
                        let unsigned = self.is_unsigned_expr(lhs);
                        self.builder.build_right_shift(l.into_int_value(), r.into_int_value(), !unsigned, "shr").unwrap().into()
                    }
                    BinOp::NullCoalesce => {
                        // `a ?? b`: `a` is an Optional `{value, present}`
                        // struct (or legacy int/pointer zero-check).
                        if l.is_struct_value() {
                            let pair = l.into_struct_value();
                            let present = self.builder.build_extract_value(pair, 1, "opt.present").unwrap().into_int_value();
                            let inner = self.builder.build_extract_value(pair, 0, "opt.value").unwrap();
                            let fallback = self.coerce_to_ty(r, inner.get_type());
                            self.builder.build_select(present, inner, fallback, "coalesce").unwrap().into()
                        } else if l.is_pointer_value() {
                            let is_null = self.builder.build_is_null(l.into_pointer_value(), "isnull").unwrap();
                            // For now, just return l if not zero else r
                            let cond = is_null;
                            // Use select
                            self.builder.build_select(cond, r, l, "coalesce").unwrap().into()
                        } else {
                            let is_null = self.builder.build_int_compare(inkwell::IntPredicate::EQ, l.into_int_value(), self.context.i64_type().const_int(0,false), "isnull").unwrap();
                            // For now, just return l if not zero else r
                            let cond = is_null;
                            // Use select
                            self.builder.build_select(cond, r, l, "coalesce").unwrap().into()
                        }
                    },
                    BinOp::Range | BinOp::RangeInclusive => {
                        // For MVP, range as array of two ints [start, end] stored as struct {i64,i64} or just return l
                        // Create struct {i64,i64}
                        let struct_ty = self.context.struct_type(&[self.context.i64_type().into(), self.context.i64_type().into()], false);
                        let mut agg: BasicValueEnum = struct_ty.get_undef().into();
                        let tmp = self.builder.build_insert_value(agg.into_struct_value(), l, 0, "range.start").unwrap();
                        agg = tmp.as_basic_value_enum();
                        let tmp2 = self.builder.build_insert_value(agg.into_struct_value(), r, 1, "range.end").unwrap();
                        agg = tmp2.as_basic_value_enum();
                        agg.into()
                    },
                    BinOp::CompoundAdd | BinOp::CompoundSub | BinOp::CompoundMul | BinOp::CompoundDiv | BinOp::CompoundMod | BinOp::CompoundBitAnd | BinOp::CompoundBitOr | BinOp::CompoundBitXor | BinOp::CompoundShl | BinOp::CompoundShr => {
                        // Should not reach here as compound is CompoundAssign, not Binary
                        l
                    },
                })
            }
            ExprKind::Conditional { cond, then_branch, else_branch } => {
                let cond_val = self.codegen_expr(cond)?.into_int_value();
                let func = self.cur_fn.unwrap();
                let then_bb = self.context.append_basic_block(func, "cond.then");
                let else_bb = self.context.append_basic_block(func, "cond.else");
                let merge_bb = self.context.append_basic_block(func, "cond.merge");
                self.builder.build_conditional_branch(cond_val, then_bb, else_bb).unwrap();
                // Evaluate the taken branch first to learn the result type
                // (branch values may be `own` pairs or structs, not just
                // ints), then materialize the entry-block result slot.
                self.builder.position_at_end(then_bb);
                let then_val = self.codegen_expr(then_branch)?;
                let result_ty = then_val.get_type();
                let result_ptr = self.create_entry_block_alloca("cond.result", result_ty);
                self.builder.build_store(result_ptr, then_val).unwrap();
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                self.builder.position_at_end(else_bb);
                let else_val = self.codegen_expr(else_branch)?;
                let else_coerced = self.coerce_to_ty(else_val, result_ty);
                self.builder.build_store(result_ptr, else_coerced).unwrap();
                self.builder.build_unconditional_branch(merge_bb).unwrap();
                self.builder.position_at_end(merge_bb);
                Ok(self.builder.build_load(result_ty, result_ptr, "cond.result.load").unwrap())
            }
            ExprKind::Range { start, end, inclusive } => {
                // For `a..b` or `a..=b` or `..b` etc. Return struct {start,end} or array
                let start_val = if let Some(s) = start { self.codegen_expr(s)? } else { self.context.i64_type().const_int(0,false).into() };
                let end_val = if let Some(e) = end { self.codegen_expr(e)? } else { self.context.i64_type().const_int(0,false).into() };
                let struct_ty = self.context.struct_type(&[self.context.i64_type().into(), self.context.i64_type().into(), self.context.bool_type().into()], false);
                let mut agg: BasicValueEnum = struct_ty.get_undef().into();
                let tmp = self.builder.build_insert_value(agg.into_struct_value(), start_val, 0, "range.start").unwrap();
                agg = tmp.as_basic_value_enum();
                let tmp2 = self.builder.build_insert_value(agg.into_struct_value(), end_val, 1, "range.end").unwrap();
                agg = tmp2.as_basic_value_enum();
                let inc = self.context.bool_type().const_int(if *inclusive { 1 } else { 0 }, false);
                let tmp3 = self.builder.build_insert_value(agg.into_struct_value(), inc, 2, "range.inclusive").unwrap();
                Ok(tmp3.as_basic_value_enum())
            }
            ExprKind::CompoundAssign { op, lhs, value } => {
                let rhs = self.codegen_expr(value)?;
                let ptr = self.codegen_as_ptr(lhs)?;
                let lhs_val = self.builder.build_load(self.context.i64_type(), ptr, "compound.load").unwrap().into_int_value();
                let rhs_val = rhs.into_int_value();
                let res = match op {
                    BinOp::CompoundAdd => self.builder.build_int_add(lhs_val, rhs_val, "compound.add").unwrap(),
                    BinOp::CompoundSub => self.builder.build_int_sub(lhs_val, rhs_val, "compound.sub").unwrap(),
                    BinOp::CompoundMul => self.builder.build_int_mul(lhs_val, rhs_val, "compound.mul").unwrap(),
                    BinOp::CompoundDiv => self.builder.build_int_signed_div(lhs_val, rhs_val, "compound.div").unwrap(),
                    BinOp::CompoundMod => self.builder.build_int_signed_rem(lhs_val, rhs_val, "compound.mod").unwrap(),
                    BinOp::CompoundBitAnd => self.builder.build_and(lhs_val, rhs_val, "compound.and").unwrap(),
                    BinOp::CompoundBitOr => self.builder.build_or(lhs_val, rhs_val, "compound.or").unwrap(),
                    BinOp::CompoundBitXor => self.builder.build_xor(lhs_val, rhs_val, "compound.xor").unwrap(),
                    BinOp::CompoundShl => self.builder.build_left_shift(lhs_val, rhs_val, "compound.shl").unwrap(),
                    BinOp::CompoundShr => self.builder.build_right_shift(lhs_val, rhs_val, false, "compound.shr").unwrap(),
                    _ => lhs_val,
                };
                self.builder.build_store(ptr, res).unwrap();
                Ok(res.into())
            }
            ExprKind::NullableMemberAccess { object, field, field_span: _ } => {
                // For `a?.b`, if a is null (0), return null/zero, else normal member access
                let obj_val = self.codegen_expr(object)?;
                // Check if object is pointer and null
                if obj_val.is_pointer_value() {
                    let is_null = self.builder.build_is_null(obj_val.into_pointer_value(), "isnull").unwrap();
                    let func = self.cur_fn.unwrap();
                    let then_bb = self.context.append_basic_block(func, "nullable.then");
                    let else_bb = self.context.append_basic_block(func, "nullable.else");
                    let merge_bb = self.context.append_basic_block(func, "nullable.merge");
                    self.builder.build_conditional_branch(is_null, else_bb, then_bb).unwrap();
                    self.builder.position_at_end(then_bb);
                    // Normal access: need field pointer
                    let field_ptr = self.codegen_field_ptr(object, field).unwrap();
                    let field_val = self.builder.build_load(self.context.i64_type(), field_ptr, "nullable.field").unwrap();
                    self.builder.build_unconditional_branch(merge_bb).unwrap();
                    self.builder.position_at_end(else_bb);
                    let null_val = self.context.i64_type().const_int(0,false);
                    self.builder.build_unconditional_branch(merge_bb).unwrap();
                    self.builder.position_at_end(merge_bb);
                    let phi = self.builder.build_phi(self.context.i64_type(), "nullable.result").unwrap();
                    phi.add_incoming(&[(&field_val, then_bb), (&null_val, else_bb)]);
                    Ok(phi.as_basic_value())
                } else {
                    // Trait-typed receiver: same switch dispatch as `.`
                    // (pairs are never null in this representation).
                    if let Ok(obj_ty) = self.infer_expr_ty(object) {
                        if let crate::sema::Ty::Struct(ref sname) = obj_ty {
                            if self.trait_names.contains(sname) {
                                return self.codegen_trait_field_load(
                                    object, sname, field, expr.span,
                                );
                            }
                        }
                    }
                    // For non-pointer, just normal access
                    let field_ptr = self.codegen_field_ptr(object, field)?;
                    Ok(self.builder.build_load(self.context.i64_type(), field_ptr, field).unwrap())
                }
            }
            ExprKind::Assign { lhs, value } => {
                let val = self.codegen_expr(value)?;
                match &lhs.kind {
                    ExprKind::Ident(name) => {
                        let (ptr, dest_ty) =
                            self.lookup_var(name).ok_or(CodegenError {
                                message: format!("undefined var {name}"),
                                span: lhs.span,
                            })?;
                        // Destroy old owned content before overwriting
                        // (avoid leak): `own` pairs plus structs with
                        // transitive `own` fields.
                        if dest_ty.is_struct_type() {
                            let st = dest_ty.into_struct_type();
                            if let Some(inner) = self.pair_owner_of(st) {
                                // Only for `own` slots (pair types).
                                self.emit_own_destroy(ptr, &inner);
                            } else if let Ok(sname) = self.ty_to_struct_name(&dest_ty) {
                                if self.struct_needs_field_destroy(&sname) {
                                    self.emit_struct_field_destroy(ptr, &sname);
                                }
                            }
                        }
                        let val = self.box_trait_value(val, dest_ty, expr.span)?;
                        let coerced = self.coerce_to_ty(val, dest_ty);
                        self.builder.build_store(ptr, coerced).unwrap();
                        // Move from owned source: null the source slot(s).
                        // Transparent through `?:`/parens/match arms; skips
                        // the destination itself (self-assignment guard).
                        // Covers `own` pairs and structs with `own` fields.
                        if dest_ty.is_struct_type() {
                            let st = dest_ty.into_struct_type();
                            let dest_owned = self.pair_owner_of(st).is_some()
                                || self.ty_to_struct_name(&dest_ty).map(|n| self.struct_needs_field_destroy(&n)).unwrap_or(false);
                            if dest_owned {
                                self.null_moved_sources(value, Some(name.as_str()));
                            }
                        }
                        Ok(coerced)
                    }
                    ExprKind::MemberAccess { object, field, .. } => {
                        // Check for property setter first
                        if let Ok(obj_ty) = self.infer_expr_ty(object) {
                            if let crate::sema::Ty::Struct(ref sname) = obj_ty {
                                if let Some(props) = self.class_properties.get(sname) {
                                    if let Some(prop) = props.get(field) {
                                        if let Some((setter, _)) = &prop.setter {
                                            let this_ptr = self.codegen_as_ptr(object)?;
                                            self.builder.build_call(*setter, &[this_ptr.into(), val.into()], &format!("set_{field}")).unwrap();
                                            return Ok(val);
                                        }
                                    }
                                }
                                // Trait-typed base: switch over implementors
                                // for the field pointer, boxing into the
                                // agreed field type.
                                if self.trait_names.contains(sname) {
                                    let field_ptr = self.codegen_trait_field_ptr(
                                        object, sname, field, expr.span,
                                    )?;
                                    let first = self
                                        .implementors_of(sname)
                                        .into_iter()
                                        .next()
                                        .ok_or(CodegenError {
                                            message: format!(
                                                "trait `{sname}` has no implementors"
                                            ),
                                            span: expr.span,
                                        })?;
                                    let field_ty = self.class_field_llvm_ty(
                                        &first, field, expr.span,
                                    )?;
                                    let val =
                                        self.box_trait_value(val, field_ty, expr.span)?;
                                    self.builder.build_store(field_ptr, val).unwrap();
                                    return Ok(val);
                                }
                            }
                        }
                        let field_ptr =
                            self.codegen_field_ptr(object, field)?;
                        self.builder.build_store(field_ptr, val).unwrap();
                        Ok(val)
                    }
                    ExprKind::Index { object, index } => {
                        // Map insert/update: `m[k] = v` writes vals[slot] on a
                        // hit, else appends (capacity-trapped like `push`).
                        if let ExprKind::Ident(name) = &object.kind {
                            if self.is_map_var(name) {
                                if let Some((ptr, ty)) = self.lookup_var(name) {
                                    if ty.is_struct_type() {
                                        let map_st = ty.into_struct_type();
                                        let key_val = self.codegen_expr(index)?;
                                        let val_in = self.codegen_expr(value)?;
                                        let (idx_res, _keys, vals_arr_ty, val_ty) =
                                            self.codegen_map_search(ptr, map_st, key_val, expr.span)?;
                                        let vals_ptr = self.builder.build_struct_gep(map_st, ptr, 1, "map.set.vals").unwrap();
                                        let zero = self.context.i64_type().const_int(0, false);
                                        let func = self.cur_fn.ok_or(CodegenError{message: "map access outside function".into(), span: expr.span})?;
                                        let hit_bb = self.context.append_basic_block(func, "map.set.hit");
                                        let miss_bb = self.context.append_basic_block(func, "map.set.miss");
                                        let merge_bb = self.context.append_basic_block(func, "map.set.merge");
                                        let idx = self.builder.build_load(self.context.i64_type(), idx_res, "map.set.idx").unwrap().into_int_value();
                                        let is_hit = self.builder.build_int_compare(IntPredicate::SGE, idx, self.context.i64_type().const_zero(), "map.set.found").unwrap();
                                        self.builder.build_conditional_branch(is_hit, hit_bb, miss_bb).unwrap();
                                        // hit: vals[idx] = v
                                        self.builder.position_at_end(hit_bb);
                                        let cv = self.coerce_to_ty(val_in, val_ty);
                                        let hptr = unsafe {
                                            self.builder.build_gep(vals_arr_ty, vals_ptr, &[zero, idx], "map.set.slot").unwrap()
                                        };
                                        self.builder.build_store(hptr, cv).unwrap();
                                        self.builder.build_unconditional_branch(merge_bb).unwrap();
                                        // miss: append key+value at len (trap past capacity)
                                        self.builder.position_at_end(miss_bb);
                                        let len_ptr = self.builder.build_struct_gep(map_st, ptr, 2, "map.set.len.ptr").unwrap();
                                        let len = self.builder.build_load(self.context.i64_type(), len_ptr, "map.set.len").unwrap().into_int_value();
                                        let cap = self.context.i64_type().const_int(Self::MAP_CAP as u64, false);
                                        let ok = self.builder.build_int_compare(IntPredicate::ULT, len, cap, "map.cap.ok").unwrap();
                                        let ok_bb = self.context.append_basic_block(func, "map.set.ok");
                                        let fail_bb = self.context.append_basic_block(func, "map.set.fail");
                                        self.builder.build_conditional_branch(ok, ok_bb, fail_bb).unwrap();
                                        self.builder.position_at_end(fail_bb);
                                        self.builder.build_call(self.get_or_declare_abort(), &[], "map.set.abort").unwrap();
                                        self.builder.build_unreachable().unwrap();
                                        self.builder.position_at_end(ok_bb);
                                        // Re-derive key slot type from the map layout.
                                        let keys_arr_ty = match map_st.get_field_type_at_index(0).unwrap() {
                                            BasicTypeEnum::ArrayType(at) => at,
                                            _ => return Err(CodegenError{message: "malformed map keys buffer".into(), span: object.span}),
                                        };
                                        let key_slot_ty = keys_arr_ty.get_element_type();
                                        let keys_ptr = self.builder.build_struct_gep(map_st, ptr, 0, "map.set.keys").unwrap();
                                        let ck = self.coerce_to_ty(key_val, key_slot_ty);
                                        let kptr = unsafe {
                                            self.builder.build_gep(keys_arr_ty, keys_ptr, &[zero, len], "map.set.key").unwrap()
                                        };
                                        self.builder.build_store(kptr, ck).unwrap();
                                        let cv2 = self.coerce_to_ty(val_in, val_ty);
                                        let vptr = unsafe {
                                            self.builder.build_gep(vals_arr_ty, vals_ptr, &[zero, len], "map.set.val").unwrap()
                                        };
                                        self.builder.build_store(vptr, cv2).unwrap();
                                        let one = self.context.i64_type().const_int(1, false);
                                        let nlen = self.builder.build_int_add(len, one, "map.len.inc").unwrap();
                                        self.builder.build_store(len_ptr, nlen).unwrap();
                                        self.builder.build_unconditional_branch(merge_bb).unwrap();
                                        self.builder.position_at_end(merge_bb);
                                        return Ok(val_in);
                                    }
                                }
                                return Err(CodegenError{message: format!("`{name}` is not a writable map"), span: object.span});
                            }
                        }
                        // arr[idx] = val  -> GEP store
                        let idx_val =
                            self.codegen_expr(index)?.into_int_value();
                        // Handle Ident array
                        if let ExprKind::Ident(name) = &object.kind {
                            if let Some((ptr, ty)) = self.lookup_var(name) {
                                if ty.is_array_type() {
                                    let arr_ty = ty.into_array_type();
                                    let elem_ptr = unsafe {
                                        self.builder
                                            .build_gep(
                                                arr_ty,
                                                ptr,
                                                &[
                                                    self.context
                                                        .i64_type()
                                                        .const_int(0, false),
                                                    idx_val,
                                                ],
                                                "idx.store",
                                            )
                                            .unwrap()
                                    };
                                    // Destroy old owned content, then move the
                                    // new value in (both no-ops for plain data).
                                    let elem_ty = arr_ty.get_element_type();
                                    if self.type_has_own_pair(&elem_ty, &mut HashSet::new()) {
                                        self.emit_field_destroy_for_ty(elem_ptr, elem_ty, 0);
                                    }
                                    self.builder
                                        .build_store(elem_ptr, val)
                                        .unwrap();
                                    self.null_moved_sources(value, None);
                                    return Ok(val);
                                } else if self.is_vec_var(name) && ty.is_struct_type() {
                                    // vec[idx] = val -> buffer GEP store (length unchanged).
                                    let vec_st = ty.into_struct_type();
                                    let buf_ptr = self.builder.build_struct_gep(vec_st, ptr, 0, "vec.buf").unwrap();
                                    let buf_field_ty = vec_st.get_field_type_at_index(0).unwrap();
                                    let buf_arr_ty = match buf_field_ty {
                                        BasicTypeEnum::ArrayType(at) => at,
                                        _ => return Err(CodegenError{message: "malformed vector buffer".into(), span: object.span}),
                                    };
                                    let elem_ty = buf_arr_ty.get_element_type();
                                    let cv = self.coerce_to_ty(val, elem_ty);
                                    let elem_ptr = unsafe {
                                        self.builder
                                            .build_gep(
                                                buf_arr_ty,
                                                buf_ptr,
                                                &[
                                                    self.context.i64_type().const_int(0, false),
                                                    idx_val,
                                                ],
                                                "vec.idx.store",
                                            )
                                            .unwrap()
                                    };
                                    if self.type_has_own_pair(&elem_ty, &mut HashSet::new()) {
                                        self.emit_field_destroy_for_ty(elem_ptr, elem_ty, 0);
                                    }
                                    self.builder.build_store(elem_ptr, cv).unwrap();
                                    self.null_moved_sources(value, None);
                                    return Ok(cv);
                                } else if ty.is_pointer_type() {
                                    let loaded = self
                                        .builder
                                        .build_load(ty, ptr, "ptr.load")
                                        .unwrap()
                                        .into_pointer_value();
                                    if self.is_string_var(name) {
                                        let elem_ptr = unsafe {
                                            self.builder
                                                .build_gep(
                                                    self.context.i8_type(),
                                                    loaded,
                                                    &[idx_val],
                                                    "idx.ptr.store",
                                                )
                                                .unwrap()
                                        };
                                        // `val` is char (i32) -> truncate to i8 for storage
                                        let byte = if val.is_int_value() {
                                            let iv = val.into_int_value();
                                            if iv.get_type().get_bit_width() == 32 {
                                                self.builder
                                                    .build_int_truncate(
                                                        iv,
                                                        self.context.i8_type(),
                                                        "str.trunc",
                                                    )
                                                    .unwrap()
                                                    .into()
                                            } else {
                                                val
                                            }
                                        } else {
                                            val
                                        };
                                        self.builder.build_store(elem_ptr, byte).unwrap();
                                        return Ok(val);
                                    } else {
                                        let elem_ptr = unsafe {
                                            self.builder
                                                .build_gep(
                                                    self.context.i64_type(),
                                                    loaded,
                                                    &[idx_val],
                                                    "idx.ptr.store",
                                                )
                                                .unwrap()
                                        };
                                        self.builder.build_store(elem_ptr, val).unwrap();
                                        return Ok(val);
                                    }
                                }
                            }
                        }
                        return Err(CodegenError{message: "unsupported indexing assignment base; only direct array variable indexing supported".into(), span: lhs.span});
                    }
                    _ => Err(CodegenError {
                        message: "invalid assignment target".into(),
                        span: lhs.span,
                    }),
                }
            }
            ExprKind::Call {
                callee,
                callee_span: _,
                args,
                type_args: _,
            } => {
                // NOTE (real stdlib): no `print`-family fast path. Calls to
                // `std::io` functions lower through the ordinary function /
                // extern resolution below.
                // Compiler intrinsic: `__hella_progname()` reads the
                // `__hella_argv0` global captured in main's prologue. It is
                // declared as an ordinary `extern` in stdlib (sema untouched)
                // and the call is never emitted, so no libc symbol is needed.
                if callee == "__hella_progname" {
                    let g = self.argv0_global();
                    let ptr_ty = self.context.ptr_type(inkwell::AddressSpace::default());
                    let v = self.builder.build_load(ptr_ty, g, "progname").unwrap();
                    return Ok(v.into());
                }
                // Check for class constructor call: ClassName(args) -> allocate + ctor
                if let Some(ctors) = self.class_constructors.get(callee).cloned() {
                    // pick ctor by arity, allowing omitted trailing defaults
                    let mut chosen = None;
                    for (func, info) in &ctors {
                        let min = info.params.len().saturating_sub(info.param_defaults.iter().rev().take_while(|d| d.is_some()).count());
                        // params[0] is `this`
                        if args.len() + 1 >= min && args.len() + 1 <= info.params.len() {
                            chosen = Some(*func);
                            break;
                        }
                    }
                    let ctor_func = chosen.or_else(|| ctors.first().map(|(f,_)| *f)).unwrap();
                    let ctor_info = ctors.iter().find(|(f, _)| *f == ctor_func).map(|(_, i)| i.clone());
                    let st = *self.struct_types.get(callee).unwrap();
                    let tmp = self.builder.build_alloca(st, "ctor.tmp").unwrap();
                    let mut arg_vals: Vec<inkwell::values::BasicMetadataValueEnum> = vec![tmp.into()];
                    match ctor_info.as_ref() {
                        // `params[0]` is `this`: positional prefix, named
                        // reorder, default fill.
                        Some(info) => arg_vals.extend(self.pack_call_args(args, info, 1)?),
                        None => {
                            for a in args {
                                arg_vals.push(self.codegen_call_arg(a)?.into());
                            }
                        }
                    }
                    self.builder.build_call(ctor_func, &arg_vals, "ctor.call").unwrap();
                    let loaded = self.builder.build_load(st.as_basic_type_enum(), tmp, "ctor.load").unwrap();
                    return Ok(loaded);
                }
                // Try direct function, extern, or variable function pointer
                if let Some((func, info)) = self.funcs.get(callee).cloned() {
                    let mut arg_vals: Vec<inkwell::values::BasicMetadataValueEnum> = Vec::new();
                    let variadic_idx = info.param_is_variadic.iter().position(|&v| v);
                    let is_c_varargs = info.param_is_variadic.iter().enumerate().any(|(i, &v)| v && info.param_names.get(i).map(|n| n.is_empty()).unwrap_or(false));
                    if let Some(vidx) = variadic_idx {
                        if is_c_varargs {
                            // C varargs `...` alone: push fixed args then variadic args directly
                            for a in args { let v = self.codegen_call_arg(a)?; arg_vals.push(v.into()); }
                        } else {
                            // Hella variadic `...T vda` where `vda` is `T[]`
                            let fixed = vidx;
                            // fixed params before variadic
                            for (i, a) in args.iter().take(fixed).enumerate() {
                                // handle named if any? For variadic with named, assume positional for fixed
                                let v = self.codegen_arg_for_param(a, &info, i)?;
                                arg_vals.push(v.into());
                            }
                            // variadic tail: `vda` as `T[]` array
                            let elem_ty = info.params.get(vidx).and_then(|t| if let crate::sema::Ty::Array(el) = t { Some(&**el) } else { None }).cloned().unwrap_or(crate::sema::Ty::Int);
                            let arr_llvm_ty: BasicTypeEnum = if let Some(bt) = self.llvm_ty_for_sema(&elem_ty) {
                                match bt {
                                    BasicTypeEnum::PointerType(pt) => pt.array_type(16).into(),
                                    BasicTypeEnum::IntType(it) => it.array_type(16).into(),
                                    BasicTypeEnum::FloatType(ft) => ft.array_type(16).into(),
                                    BasicTypeEnum::StructType(st) => st.array_type(16).into(),
                                    BasicTypeEnum::ArrayType(at) => at.array_type(16).into(),
                                    _ => self.context.i64_type().array_type(16).into(),
                                }
                            } else {
                                self.context.i64_type().array_type(16).into()
                            };
                            let mut arr_val: BasicValueEnum = arr_llvm_ty.into_array_type().get_undef().into();
                            // Fill array with variadic args
                            let elem_dest: Option<BasicTypeEnum> = self.llvm_ty_for_sema(&elem_ty);
                            for (j, arg) in args.iter().skip(fixed).enumerate() {
                                self.materialize_out_var(arg, Some(&elem_ty))?;
                                let v = self.codegen_call_arg(arg)?;
                                let v = match elem_dest {
                                    Some(dest) => self.box_trait_value(v, dest, arg.span())?,
                                    None => v,
                                };
                                let idx = self.context.i32_type().const_int(j as u64, false);
                                // For array, use insert_value
                                if arr_val.is_array_value() {
                                    let tmp = self.builder.build_insert_value(arr_val.into_array_value(), v, j as u32, &format!("vararg.{}", j)).unwrap();
                                    arr_val = tmp.as_basic_value_enum();
                                } else {
                                    // For struct? Just use first
                                    arr_val = v;
                                }
                            }
                            // If no variadic args, arr_val is undef, need to make zero
                            if args.len() <= fixed {
                                arr_val = arr_llvm_ty.const_zero().into();
                            }
                            arg_vals.push(arr_val.into());
                            // Handle remaining fixed params after variadic if any (when variadic not last but explicit type allows middle)
                            // For `a, ...int vda, b` where `vda` is variadic in middle, `b` is after, we need to handle
                            // For now, assume variadic is last for derived, but for explicit middle, we need to handle
                            // For `...string vda, bool cond` with `vda` variadic in middle, `cond` is after, the variadic `vda` should consume `args[fixed.. args.len()-1]` and `cond` is last arg
                            // Detect if variadic not last: if vidx + 1 < info.params.len(), then last param is after variadic
                            if vidx + 1 < info.params.len() {
                                // For `...string vda, bool cond` with `vda` at vidx, `cond` at vidx+1, the call `log("fmt", "a", "b", true)` where `fmt` at 0, `vda` at 1 is variadic, `cond` at 2 is bool
                                // `args` is `["fmt", "a", "b", true]` with 4 args, `fixed` is vidx (1), `vda` is at 1, `cond` is at 2
                                // We already handled `vda` as array with `args[1..3]` as `["a","b"]` and `true` as `cond` should be last
                                // But our current handling for variadic `vda` as array with `args[fixed..]` as all remaining, would include `true` as part of `vda` incorrectly
                                // For explicit variadic in middle, we need to know how many args belong to `vda` vs `cond`
                                // For MVP, assume variadic `vda` consumes `args.len() - params.len() + 1` args
                                // E.g., `log(string fmt, ...string vda, bool cond)` with `fmt` at 0, `vda` at 1, `cond` at 2, `params.len()=3`, `args.len()=4` where `args` is `["fmt", "a", "b", true]` -> `vda` should be `["a","b"]` (2) and `cond` is `true` (1)
                                // So variadic element count = args.len() - params.len() + 1
                                // We already pushed `vda` as array with all remaining, but we need to handle `cond` separately
                                // For now, we already pushed `vda` as array with `args[fixed..]` (= `["a","b",true]`), which incorrectly includes `true`
                                // To fix, we need to handle variadic not last: `vda` should be `args[fixed .. args.len() - (params.len() - vidx -1)]`
                                // For `vda` at 1 with `params.len()=3`, `args.len()=4`, `vda` count = 4 -3 +1 =2, so `vda` is `args[1..3]` = `["a","b"]`, `cond` is `args[3]` = `true`
                                // We should handle this
                                let remaining_params = info.params.len() - vidx - 1;
                                let vda_count = args.len() - info.params.len() + 1;
                                // Rebuild arg_vals without the incorrect vda, and fix
                                // For now, pop the incorrectly built vda and rebuild
                                arg_vals.pop();
                                // Rebuild vda with correct count
                                let mut arr_val2: BasicValueEnum = arr_llvm_ty.const_zero().into();
                                let elem_dest2: Option<BasicTypeEnum> = self.llvm_ty_for_sema(&elem_ty);
                                for (j, arg) in args.iter().skip(fixed).take(vda_count).enumerate() {
                                    self.materialize_out_var(arg, Some(&elem_ty))?;
                                    let v = self.codegen_call_arg(arg)?;
                                    let v = match elem_dest2 {
                                        Some(dest) => self.box_trait_value(v, dest, arg.span())?,
                                        None => v,
                                    };
                                    if arr_val2.is_array_value() {
                                        let tmp = self.builder.build_insert_value(arr_val2.into_array_value(), v, j as u32, &format!("vararg.fix.{}", j)).unwrap();
                                        arr_val2 = tmp.as_basic_value_enum();
                                    }
                                }
                                arg_vals.push(arr_val2.into());
                                // Push remaining fixed after variadic
                                for (j, arg) in args.iter().skip(fixed + vda_count).enumerate() {
                                    let v = self.codegen_arg_for_param(arg, &info, vidx + 1 + j)?;
                                    arg_vals.push(v.into());
                                }
                            }
                        }
                    } else {
                        // Positional prefix, named reorder, default fill.
                        arg_vals.extend(self.pack_call_args(args, &info, 0)?);
                    }
                    // Async-6: an `async` callee is never invoked inline.
                    // Spawn its body on a worker thread and return the
                    // `task<Ret>` handle immediately; `await` joins it.
                    if info.is_async {
                        return self.codegen_async_spawn_call(func, &info, arg_vals, expr.span);
                    }
                    let call = self.builder.build_call(func, &arg_vals, "call").unwrap();
                    let vk = call.try_as_basic_value();
                    if vk.is_basic() { return Ok(vk.basic().unwrap()); } else { return Ok(self.context.i64_type().const_int(0,false).into()); }
                }
                if let Some(f) = self.module.get_function(callee) {
                    let mut arg_vals: Vec<inkwell::values::BasicMetadataValueEnum> = Vec::new();
                    for a in args {
                        // No signature context (matches sema's `any`
                        // fallback for implicit `out` declarations).
                        self.materialize_out_var(a, None)?;
                        let v = self.codegen_call_arg(a)?;
                        arg_vals.push(v.into());
                    }
                    let call = self.builder.build_call(f, &arg_vals, "call").unwrap();
                    let vk = call.try_as_basic_value();
                    if vk.is_basic() {
                        let v = vk.basic().unwrap();
                        // Extern C `int` returns lower as i32; Hella `int`
                        // is i64, so sign-extend (not zero-extend: the sign
                        // of e.g. strcmp/scanf results must survive).
                        // Unsigned `u32` returns zero-extend instead (A4).
                        if self.extern_int32_rets.contains(callee) {
                            if let BasicValueEnum::IntValue(iv) = v {
                                if iv.get_type().get_bit_width() == 32 {
                                    return Ok(self.builder.build_int_s_extend(iv, self.context.i64_type(), "extern.sext").unwrap().into());
                                }
                            }
                        }
                        if self.extern_uint32_rets.contains(callee) {
                            if let BasicValueEnum::IntValue(iv) = v {
                                if iv.get_type().get_bit_width() == 32 {
                                    return Ok(self.builder.build_int_z_extend(iv, self.context.i64_type(), "extern.zext").unwrap().into());
                                }
                            }
                        }
                        return Ok(v);
                    } else { return Ok(self.context.i64_type().const_int(0,false).into()); }
                }
                if let Some((ptr, ty)) = self.lookup_var(callee) {
                    let loaded = self.builder.build_load(ty, ptr, "func.load").unwrap();
                    let mut arg_vals: Vec<inkwell::values::BasicMetadataValueEnum> = Vec::new();
                    let mut param_tys: Vec<inkwell::types::BasicMetadataTypeEnum> = Vec::new();
                    for a in args {
                        self.materialize_out_var(a, None)?;
                        let v = self.codegen_call_arg(a)?;
                        param_tys.push(v.get_type().into());
                        arg_vals.push(v.into());
                    }
                    let ret_ty = self.context.i64_type();
                    let fn_ty = ret_ty.fn_type(&param_tys, false);
                    let fn_ptr = loaded.into_pointer_value();
                    let call = self.builder.build_indirect_call(fn_ty, fn_ptr, &arg_vals, "indirect").unwrap();
                    let vk = call.try_as_basic_value();
                    if vk.is_basic() { return Ok(vk.basic().unwrap()); } else { return Ok(self.context.i64_type().const_int(0,false).into()); }
                }
                return Err(CodegenError { message: format!("undefined function {callee}"), span: expr.span });
            }
            ExprKind::MemberAccess {
                object,
                field,
                field_span: _,
            } => {
                // Check for enum variant `MyEnum.A` where `MyEnum` is enum and `A` is variant
                let obj_ty = self.infer_expr_ty(object)?;
                if let crate::sema::Ty::Enum(ref ename) = obj_ty {
                    if let Some(tag_map) = self.enum_variant_tags.get(ename) {
                        if let Some(tag) = tag_map.get(field) {
                            let enum_ty = self.enum_types.get(ename).unwrap();
                            let mut agg: BasicValueEnum<'ctx> = enum_ty.get_undef().into();
                            let tag_val = self.context.i32_type().const_int(*tag as u64, false);
                            let tmp = self.builder.build_insert_value(agg.into_struct_value(), tag_val, 0, "enum.tag").unwrap();
                            agg = tmp.as_basic_value_enum();
                            // payload remains zero (no args for `MyEnum.A`)
                            return Ok(agg);
                        }
                    }
                }
                // Check for property getter first
                if let crate::sema::Ty::Struct(ref sname) = obj_ty {
                    if let Some(props) = self.class_properties.get(sname) {
                        if let Some(prop) = props.get(field) {
                            if let Some((getter, _)) = &prop.getter {
                                // call getter(this)
                                let this_ptr = self.codegen_as_ptr(object)?;
                                let call = self.builder.build_call(*getter, &[this_ptr.into()], &format!("get_{field}")).unwrap();
                                let ret = call.try_as_basic_value();
                                if ret.is_basic() {
                                    return Ok(ret.basic().unwrap());
                                } else {
                                    return Ok(self.context.i64_type().const_int(0,false).into());
                                }
                            }
                        }
                    }
                }
                // Trait-typed receiver: switch over implementors (layouts
                // may differ), GEP per layout, load the agreed field type.
                if let crate::sema::Ty::Struct(ref sname) = obj_ty {
                    if self.trait_names.contains(sname) {
                        return self.codegen_trait_field_load(object, sname, field, expr.span);
                    }
                }
                // rvalue field load: need field pointer then load
                let field_ptr = self.codegen_field_ptr(object, field)?;
                let obj_ty2 = self.infer_expr_ty(object)?;
                if let crate::sema::Ty::Struct(ref sname) = obj_ty2 {
                    let fields = self.struct_fields.get(sname).unwrap();
                    let idx = *fields.get(field).unwrap();
                    let st = self.struct_types.get(sname).unwrap();
                    let field_ty = st.get_field_type_at_index(idx).unwrap();
                    Ok(self
                        .builder
                        .build_load(field_ty, field_ptr, field)
                        .unwrap())
                } else {
                    Err(CodegenError {
                        message: format!("field access on non-struct"),
                        span: expr.span,
                    })
                }
            }
            ExprKind::Index { object, index } => {
                // a[i] rvalue: GEP on array or string
                // Maps take the search path (keys are values, not indices).
                if let ExprKind::Ident(name) = &object.kind {
                    if self.is_map_var(name) {
                        if let Some((ptr, ty)) = self.lookup_var(name) {
                            if ty.is_struct_type() {
                                let map_st = ty.into_struct_type();
                                let key_val = self.codegen_expr(index)?;
                                let (idx_res, _keys, vals_arr_ty, val_ty) =
                                    self.codegen_map_search(ptr, map_st, key_val, expr.span)?;
                                // hit ? vals[idx] : zero
                                let func = self.cur_fn.ok_or(CodegenError{message: "map access outside function".into(), span: expr.span})?;
                                let hit_bb = self.context.append_basic_block(func, "map.get.hit");
                                let miss_bb = self.context.append_basic_block(func, "map.get.miss");
                                let merge_bb = self.context.append_basic_block(func, "map.get.merge");
                                let res = self.create_entry_block_alloca("map.get.res", val_ty);
                                self.builder.build_store(res, val_ty.const_zero()).unwrap();
                                let idx = self.builder.build_load(self.context.i64_type(), idx_res, "map.get.idx").unwrap().into_int_value();
                                let is_hit = self.builder.build_int_compare(IntPredicate::SGE, idx, self.context.i64_type().const_zero(), "map.get.found").unwrap();
                                self.builder.build_conditional_branch(is_hit, hit_bb, miss_bb).unwrap();
                                self.builder.position_at_end(hit_bb);
                                let vals_ptr = self.builder.build_struct_gep(map_st, ptr, 1, "map.get.vals").unwrap();
                                let zero = self.context.i64_type().const_int(0, false);
                                let vptr = unsafe {
                                    self.builder.build_gep(vals_arr_ty, vals_ptr, &[zero, idx], "map.get.slot").unwrap()
                                };
                                let vv = self.builder.build_load(val_ty, vptr, "map.get.val").unwrap();
                                self.builder.build_store(res, vv).unwrap();
                                self.builder.build_unconditional_branch(merge_bb).unwrap();
                                self.builder.position_at_end(miss_bb);
                                self.builder.build_unconditional_branch(merge_bb).unwrap();
                                self.builder.position_at_end(merge_bb);
                                return Ok(self.builder.build_load(val_ty, res, "map.get").unwrap());
                            }
                        }
                        return Err(CodegenError{message: format!("`{name}` is not a readable map"), span: object.span});
                    }
                }
                let idx_val = self.codegen_expr(index)?.into_int_value();
                // Determine object type: if it's Ident array var, it's [16 x i64] alloca
                // For simplicity, handle Ident array and member access array (e.g., s.arr[i]) via GEP
                // Try lookup as variable first
                if let ExprKind::Ident(name) = &object.kind {
                    if let Some((ptr, ty)) = self.lookup_var(name) {
                        if ty.is_array_type() {
                            let arr_ty = ty.into_array_type();
                            let elem_ptr = unsafe {
                                self.builder
                                    .build_gep(
                                        arr_ty,
                                        ptr,
                                        &[
                                            self.context
                                                .i64_type()
                                                .const_int(0, false),
                                            idx_val,
                                        ],
                                        "idx",
                                    )
                                    .unwrap()
                            };
                            let elem_ty = arr_ty.get_element_type();
                            return Ok(self
                                .builder
                                .build_load(elem_ty, elem_ptr, "idx.load")
                                .unwrap());
                        } else if self.is_vec_var(name) && ty.is_struct_type() {
                            // Vector index: buffer is struct field 0.
                            let vec_st = ty.into_struct_type();
                            let buf_ptr = self.builder.build_struct_gep(vec_st, ptr, 0, "vec.buf").unwrap();
                            let buf_field_ty = vec_st.get_field_type_at_index(0).unwrap();
                            let buf_arr_ty = match buf_field_ty {
                                BasicTypeEnum::ArrayType(at) => at,
                                _ => return Err(CodegenError{message: "malformed vector buffer".into(), span: object.span}),
                            };
                            let elem_ptr = unsafe {
                                self.builder
                                    .build_gep(
                                        buf_arr_ty,
                                        buf_ptr,
                                        &[
                                            self.context.i64_type().const_int(0, false),
                                            idx_val,
                                        ],
                                        "vec.idx",
                                    )
                                    .unwrap()
                            };
                            let elem_ty = buf_arr_ty.get_element_type();
                            return Ok(self
                                .builder
                                .build_load(elem_ty, elem_ptr, "vec.idx.load")
                                .unwrap());
                        } else if ty.is_pointer_type() {
                            let loaded = self
                                .builder
                                .build_load(ty, ptr, "ptr.load")
                                .unwrap()
                                .into_pointer_value();
                            if self.is_string_var(name) {
                                let elem_ptr = unsafe {
                                    self.builder
                                        .build_gep(
                                            self.context.i8_type(),
                                            loaded,
                                            &[idx_val],
                                            "idx.ptr",
                                        )
                                        .unwrap()
                                };
                                let byte = self
                                    .builder
                                    .build_load(self.context.i8_type(), elem_ptr, "idx.load")
                                    .unwrap()
                                    .into_int_value();
                                let ext = self
                                    .builder
                                    .build_int_z_extend(
                                        byte,
                                        self.context.i32_type(),
                                        "idx.ext",
                                    )
                                    .unwrap();
                                return Ok(ext.into());
                            } else {
                                let elem_ptr = unsafe {
                                    self.builder
                                        .build_gep(
                                            self.context.i64_type(),
                                            loaded,
                                            &[idx_val],
                                            "idx.ptr",
                                        )
                                        .unwrap()
                                };
                                return Ok(self
                                    .builder
                                    .build_load(
                                        self.context.i64_type(),
                                        elem_ptr,
                                        "idx.load",
                                    )
                                    .unwrap());
                            }
                        } else if ty.is_struct_type() {
                            // string as {ptr,len} ? For now string as ptr: fallback to pointer case
                            // but string currently maps to ptr, not struct, so handle pointer case above
                        }
                    }
                }
                // Fallback: try codegen object as pointer value (if object is MemberAccess that yields pointer? For now handle general via loaded pointer)
                // For Phase 2, support a[i] where a is array variable; for other cases, error
                return Err(CodegenError{message: "unsupported indexing base; only direct array variable indexing supported in Phase 2".into(), span: expr.span});
            }
            ExprKind::Slice { object, start, end, inclusive } => {
                // `a[l..r]` / `a[..r]` / `a[l..]` / `a[..]` (`..=` includes
                // `end`). Arrays and vectors copy the clamped range (zero
                // outside); strings allocate a fresh null-terminated copy.
                // Bounds evaluate once, in order.
                let (base_ptr, base_ty): (PointerValue<'ctx>, BasicTypeEnum<'ctx>) = if let ExprKind::Ident(ref n) = object.kind {
                    let lookup = n.rsplit("::").next().unwrap_or(n);
                    self.lookup_var(n).or_else(|| self.lookup_var(lookup)).ok_or(CodegenError{message: format!("undefined variable `{n}`"), span: object.span})?
                } else {
                    let v = self.codegen_expr(object)?;
                    let tmp = self.create_entry_block_alloca("__slice_base", v.get_type());
                    self.builder.build_store(tmp, v).unwrap();
                    (tmp, v.get_type())
                };
                match base_ty {
                    BasicTypeEnum::ArrayType(arr_ty) => {
                        let n = arr_ty.len();
                        let ctx: &'ctx Context = self.context;
                        let (lo, len) = self.slice_bounds(start, end, *inclusive, ctx.i64_type().const_int(n as u64, false))?;
                        let i64_ty = ctx.i64_type();
                        let elem_ty = arr_ty.get_element_type();
                        let zero_elem = self.slice_zero_elem(elem_ty, object.span)?;
                        let mut agg: BasicValueEnum<'ctx> = arr_ty.get_undef().into();
                        for i in 0..n {
                            let idx = i64_ty.const_int(i as u64, false);
                            let in_range = self.builder.build_int_compare(IntPredicate::SLT, idx, len, "slice.in").unwrap();
                            let want = self.builder.build_int_add(lo, idx, "slice.want").unwrap();
                            let src_idx = self.builder.build_select(in_range, want, i64_ty.const_zero(), "slice.src").unwrap().into_int_value();
                            let elem_ptr = unsafe { self.builder.build_gep(arr_ty, base_ptr, &[i64_ty.const_zero(), src_idx], "slice.elem.ptr").unwrap() };
                            let e = self.builder.build_load(elem_ty, elem_ptr, "slice.elem").unwrap();
                            let picked = self.builder.build_select(in_range, e, zero_elem, "slice.pick").unwrap();
                            let tmp = self.builder.build_insert_value(agg.into_array_value(), picked, i, "slice.ins").unwrap();
                            agg = tmp.as_basic_value_enum();
                        }
                        Ok(agg)
                    }
                    BasicTypeEnum::StructType(vec_st) if vec_st.count_fields() == 2 => {
                        // Vector `{buf, len}`: copy the live prefix range into
                        // a fresh vector value (buffer zero-filled past it).
                        let buf_field = vec_st.get_field_type_at_index(0).unwrap();
                        let buf_arr_ty = match buf_field {
                            BasicTypeEnum::ArrayType(at) => at,
                            _ => return Err(CodegenError{message: "slicing is only supported on arrays, vectors, and strings".into(), span: object.span}),
                        };
                        let cap = buf_arr_ty.len();
                        let elem_ty = buf_arr_ty.get_element_type();
                        let ctx: &'ctx Context = self.context;
                        let i64_ty = ctx.i64_type();
                        let len_ptr = self.builder.build_struct_gep(vec_st, base_ptr, 1, "vslice.len.ptr").unwrap();
                        let veclen = self.builder.build_load(i64_ty, len_ptr, "vslice.len").unwrap().into_int_value();
                        let (lo, len) = self.slice_bounds(start, end, *inclusive, veclen)?;
                        let zero_elem = self.slice_zero_elem(elem_ty, object.span)?;
                        let buf_ptr = self.builder.build_struct_gep(vec_st, base_ptr, 0, "vslice.buf").unwrap();
                        let mut buf_val: BasicValueEnum<'ctx> = buf_arr_ty.get_undef().into();
                        for i in 0..cap {
                            let idx = i64_ty.const_int(i as u64, false);
                            let in_range = self.builder.build_int_compare(IntPredicate::SLT, idx, len, "vslice.in").unwrap();
                            let want = self.builder.build_int_add(lo, idx, "vslice.want").unwrap();
                            let src_idx = self.builder.build_select(in_range, want, i64_ty.const_zero(), "vslice.src").unwrap().into_int_value();
                            let elem_ptr = unsafe { self.builder.build_gep(buf_arr_ty, buf_ptr, &[i64_ty.const_zero(), src_idx], "vslice.elem.ptr").unwrap() };
                            let e = self.builder.build_load(elem_ty, elem_ptr, "vslice.elem").unwrap();
                            let picked = self.builder.build_select(in_range, e, zero_elem, "vslice.pick").unwrap();
                            let tmp = self.builder.build_insert_value(buf_val.into_array_value(), picked, i, "vslice.ins").unwrap();
                            buf_val = tmp.as_basic_value_enum();
                        }
                        let mut agg: BasicValueEnum<'ctx> = vec_st.get_undef().into();
                        let tmp = self.builder.build_insert_value(agg.into_struct_value(), buf_val, 0, "vslice.buf").unwrap();
                        agg = tmp.as_basic_value_enum();
                        let tmp2 = self.builder.build_insert_value(agg.into_struct_value(), len.as_basic_value_enum(), 1, "vslice.len").unwrap();
                        agg = tmp2.as_basic_value_enum();
                        Ok(agg)
                    }
                    BasicTypeEnum::PointerType(_) => {
                        // String: fresh `malloc`'d null-terminated copy.
                        let ctx: &'ctx Context = self.context;
                        let i64_ty = ctx.i64_type();
                        let i8_ty = ctx.i8_type();
                        let ptr_ty = ctx.ptr_type(inkwell::AddressSpace::default());
                        let sptr = self.builder.build_load(ptr_ty, base_ptr, "sslice.ptr").unwrap().into_pointer_value();
                        let call = self.builder.build_call(self.get_or_declare_strlen(), &[sptr.into()], "sslice.len").unwrap();
                        let slen = call.try_as_basic_value().basic().unwrap().into_int_value();
                        let (lo, len) = self.slice_bounds(start, end, *inclusive, slen)?;
                        let total = self.builder.build_int_add(len, i64_ty.const_int(1, false), "sslice.total").unwrap();
                        let malloc = self.get_or_declare_malloc();
                        let raw = self.builder.build_call(malloc, &[total.into()], "sslice.malloc").unwrap().try_as_basic_value().basic().unwrap().into_pointer_value();
                        let src = unsafe { self.builder.build_gep(i8_ty, sptr, &[lo], "sslice.src").unwrap() };
                        let memcpy = self.get_or_declare_memcpy();
                        self.builder.build_call(memcpy, &[raw.into(), src.into(), len.into()], "sslice.copy").unwrap();
                        let end_ptr = unsafe { self.builder.build_gep(i8_ty, raw, &[len], "sslice.end").unwrap() };
                        self.builder.build_store(end_ptr, i8_ty.const_zero()).unwrap();
                        Ok(raw.as_basic_value_enum())
                    }
                    _ => Err(CodegenError{message: "slicing is only supported on arrays, vectors, and strings".into(), span: object.span}),
                }
            }
            ExprKind::StringLit(s) => {
                // Create global string pointer: build_global_string_ptr returns i8* to null-terminated string
                let ptr =
                    self.builder.build_global_string_ptr(s, "str.lit").unwrap();
                // string type is ptr (i8*), return ptr
                Ok(ptr.as_pointer_value().into())
            }
            ExprKind::CharLit(ch) => {
                // char as i32 Unicode scalar
                Ok(self.context.i32_type().const_int(*ch as u64, false).into())
            }
            ExprKind::StructLit { ty, fields } => {
                let sname = match ty {
                    Type::Named(n, _) => n.clone(),
                    _ => {
                        return Err(CodegenError {
                            message: "struct literal requires named type"
                                .into(),
                            span: expr.span,
                        });
                    }
                };
                let st =
                    *self.struct_types.get(&sname).ok_or(CodegenError {
                        message: format!("unknown struct {sname}"),
                        span: expr.span,
                    })?;
                let field_map = self.struct_fields.get(&sname).unwrap().clone();
                // Allocate temp struct on stack, fill fields, load aggregate value
                let tmp =
                    self.builder.build_alloca(st, "struct.lit.tmp").unwrap();
                // Zero-initialize to handle missing fields without default (undef would be bad)
                self.builder.build_store(tmp, st.const_zero()).unwrap();
                for (fname, _fspan, fexpr) in fields {
                    let idx = *field_map.get(fname).ok_or(CodegenError {
                        message: format!("unknown field {fname}"),
                        span: expr.span,
                    })?;
                    let val = self.codegen_expr(fexpr)?;
                    let field_ptr = self
                        .builder
                        .build_struct_gep(st, tmp, idx, &format!("s.{}", fname))
                        .unwrap();
                    let dest = st.get_field_type_at_index(idx).unwrap();
                    let val = self.box_trait_value(val, dest, fexpr.span)?;
                    self.builder.build_store(field_ptr, val).unwrap();
                    // Moving into an owned field nulls the source slot(s).
                    if dest.is_struct_type() {
                        let dst = dest.into_struct_type();
                        let owned = self.pair_owner_of(dst).is_some()
                            || self.ty_to_struct_name(&dest).map(|n| self.struct_needs_field_destroy(&n)).unwrap_or(false);
                        if owned {
                            self.null_moved_sources(fexpr, None);
                        }
                    }
                }
                // Fill missing fields with defaults if any
                let provided: std::collections::HashSet<String> = fields.iter().map(|(n, _, _)| n.clone()).collect();
                let defaults_opt = self.struct_field_defaults.get(&sname).cloned();
                if let Some(defaults) = defaults_opt {
                    for (fname, idx) in field_map.iter() {
                        if !provided.contains(fname) {
                            if let Some(def_expr) = defaults.get(fname) {
                                let val = self.codegen_expr(def_expr)?;
                                let field_ptr = self.builder.build_struct_gep(st, tmp, *idx, &format!("s.{}_default", fname)).unwrap();
                                self.builder.build_store(field_ptr, val).unwrap();
                            }
                        }
                    }
                }
                let loaded = self
                    .builder
                    .build_load(st.as_basic_type_enum(), tmp, "struct.lit")
                    .unwrap();
                Ok(loaded)
            }
            ExprKind::EnumVariant{enum_name, variant, variant_span: _, args} => {
                // Enum variant construction: produce {tag, payload} struct value
                let vbase: &str = variant.rsplit("::").next().unwrap_or(variant);
                let ename = if let Some(n) = enum_name { n.clone() } else {
                    // search for enum containing variant
                    let mut found = None;
                    for (ename, einfo) in &self.enum_variant_tags {
                        if einfo.contains_key(vbase) { found = Some(ename.clone()); break; }
                    }
                    found.unwrap_or_else(|| variant.clone())
                };
                let tag_map = self.enum_variant_tags.get(&ename).ok_or(CodegenError{message: format!("unknown enum `{ename}`"), span: expr.span})?;
                let tag = *tag_map.get(vbase).ok_or(CodegenError{message: format!("unknown variant `{variant}` for enum `{ename}`"), span: expr.span})? as u64;
                let enum_ty = self.enum_types.get(&ename).ok_or(CodegenError{message: format!("unknown enum `{ename}`"), span: expr.span})?;
                // start with undef, insert tag at 0, payload at 1 if present
                let mut agg: BasicValueEnum<'ctx> = enum_ty.get_undef().into();
                let tag_val = self.context.i32_type().const_int(tag, false);
                let tmp = self.builder.build_insert_value(agg.into_struct_value(), tag_val, 0, "enum.tag").unwrap();
                agg = tmp.as_basic_value_enum();
                if !args.is_empty() {
                    if args.len() > 1 {
                        // Wide payload: pack each arg's words after its tag.
                        // (Legacy `{tag, i64}` enums never reach here through
                        // sema: arity is checked at construction.)
                        let slot_probe = self.builder.build_extract_value(agg.into_struct_value(), 1, "enum.payload.arr").unwrap();
                        if !slot_probe.is_array_value() {
                            return Err(CodegenError{message: format!("variant `{variant}` has {} payload args but `{ename}` is not a wide-payload enum", args.len()), span: expr.span});
                        }
                        let ftys = self.enum_payload_tys.get(&ename).and_then(|m| m.get(vbase)).cloned().unwrap_or_default();
                        let ctx: &'ctx Context = self.context;
                        let i64_ty = ctx.i64_type();
                        let mut cursor = 0u32;
                        for (i, a) in args.iter().enumerate() {
                            let want_ty = ftys.get(i).cloned().unwrap_or(i64_ty.into());
                            let mut v = self.codegen_call_arg(a)?;
                            v = self.coerce_to_ty(v, want_ty);
                            let vty = v.get_type();
                            let k = Self::llvm_word_count(&vty).unwrap_or(1).max(1) as u32;
                            let tmp_alloc = self.create_entry_block_alloca("enum.payload.tmp", vty);
                            self.builder.build_store(tmp_alloc, v).unwrap();
                            let opaque_ptr: BasicTypeEnum<'ctx> = ctx.ptr_type(inkwell::AddressSpace::default()).into();
                            let word_ptr = self.builder.build_bit_cast(tmp_alloc.as_basic_value_enum(), opaque_ptr, "enum.payload.words").unwrap().into_pointer_value();
                            for w in 0..k {
                                let idx = i64_ty.const_int(w as u64, false);
                                let wptr = unsafe { self.builder.build_gep(i64_ty, word_ptr, &[idx], "enum.payload.word.ptr").unwrap() };
                                let word = self.builder.build_load(i64_ty, wptr, "enum.payload.word").unwrap();
                                let slot = self.builder.build_extract_value(agg.into_struct_value(), 1, "enum.payload.arr").unwrap();
                                let one = self.builder.build_insert_value(slot.into_array_value(), word, cursor, "enum.payload.set").unwrap();
                                let back = self.builder.build_insert_value(agg.into_struct_value(), one.as_basic_value_enum(), 1, "enum.payload.arr").unwrap();
                                agg = back.as_basic_value_enum();
                                cursor += 1;
                            }
                        }
                    } else {
                        let payload_val = self.codegen_call_arg(&args[0])?;
                        let tmp2 = self.builder.build_insert_value(agg.into_struct_value(), payload_val, 1, "enum.payload").unwrap();
                        agg = tmp2.as_basic_value_enum();
                    }
                }
                Ok(agg)
            }
            ExprKind::Match(m) => self.codegen_match(m, expr.span),
            ExprKind::InterpolatedString(parts, _) => {
                // A3: bounded interpolation. 4KiB stack buffer, every append
                // via strncat with remaining-space computed from strlen, int/
                // float temps via snprintf. Overlong text truncates instead
                // of smashing the stack; exact-size building belongs in
                // std::fmt (checked allocs). 64-bit ints use %lld (portable).
                const INTERP_CAP: u64 = 4096;
                let arr_ty = self.context.i8_type().array_type(INTERP_CAP as u32);
                let buffer = self.builder.build_alloca(arr_ty, "interp.buf").unwrap();
                let buf_ptr = self.builder.build_bit_cast(buffer.as_basic_value_enum(), self.context.ptr_type(inkwell::AddressSpace::default()), "interp.ptr").unwrap().into_pointer_value();
                let first = unsafe { self.builder.build_gep(arr_ty, buffer, &[self.context.i32_type().const_int(0,false), self.context.i32_type().const_int(0,false)], "first").unwrap() };
                self.builder.build_store(first, self.context.i8_type().const_int(0,false)).unwrap();
                // Append helper: strncat(buf, part, CAP-1-strlen(buf)).
                // When remaining <= 0 the append is skipped (truncation).
                let append_bounded = |me: &Self, part_ptr: inkwell::values::PointerValue<'ctx>| {
                    let strlen = me.get_or_declare_strlen();
                    let strncat = me.get_or_declare_strncat();
                    let cur_len = me.builder.build_call(strlen, &[buf_ptr.into()], "interp.len").unwrap().try_as_basic_value().basic().unwrap().into_int_value();
                    let cur64 = me.builder.build_int_z_extend_or_bit_cast(cur_len, me.context.i64_type(), "interp.len64").unwrap();
                    let cap = me.context.i64_type().const_int(INTERP_CAP - 1, false);
                    let rem = me.builder.build_int_sub(cap, cur64, "interp.rem").unwrap();
                    let cur_block = me.builder.get_insert_block().unwrap();
                    let cur_fn = cur_block.get_parent().unwrap();
                    let do_cat = me.context.append_basic_block(cur_fn, "interp.cat");
                    let done = me.context.append_basic_block(cur_fn, "interp.done");
                    let positive = me.builder.build_int_compare(inkwell::IntPredicate::SGT, rem, me.context.i64_type().const_zero(), "interp.has").unwrap();
                    me.builder.build_conditional_branch(positive, do_cat, done).unwrap();
                    me.builder.position_at_end(do_cat);
                    me.builder.build_call(strncat, &[buf_ptr.into(), part_ptr.into(), rem.into()], "strncat").unwrap();
                    me.builder.build_unconditional_branch(done).unwrap();
                    me.builder.position_at_end(done);
                };
                for part in parts {
                    match part {
                        InterpolatedPart::Literal(s) => {
                            let lit_ptr = self.builder.build_global_string_ptr(s, "interp.lit").unwrap();
                            append_bounded(self, lit_ptr.as_pointer_value());
                        }
                        InterpolatedPart::Expr(e) => {
                            let val = self.codegen_expr(e)?;
                            if val.is_int_value() {
                                let int_buf = self.builder.build_alloca(self.context.i8_type().array_type(64), "intbuf").unwrap();
                                let int_ptr = self.builder.build_bit_cast(int_buf.as_basic_value_enum(), self.context.ptr_type(inkwell::AddressSpace::default()), "intptr").unwrap().into_pointer_value();
                                let fmt = self.builder.build_global_string_ptr("%lld", "fmt.int").unwrap();
                                let snprintf = self.get_or_declare_snprintf();
                                let sz = self.context.i64_type().const_int(64, false);
                                self.builder.build_call(snprintf, &[int_ptr.into(), sz.into(), fmt.as_pointer_value().into(), val.into()], "snprintf").unwrap();
                                append_bounded(self, int_ptr);
                            } else if val.is_pointer_value() {
                                append_bounded(self, val.into_pointer_value());
                            } else if val.is_float_value() {
                                let flt_buf = self.builder.build_alloca(self.context.i8_type().array_type(64), "fltbuf").unwrap();
                                let flt_ptr = self.builder.build_bit_cast(flt_buf.as_basic_value_enum(), self.context.ptr_type(inkwell::AddressSpace::default()), "fltptr").unwrap().into_pointer_value();
                                let fmt = self.builder.build_global_string_ptr("%f", "fmt.flt").unwrap();
                                let snprintf = self.get_or_declare_snprintf();
                                let sz = self.context.i64_type().const_int(64, false);
                                self.builder.build_call(snprintf, &[flt_ptr.into(), sz.into(), fmt.as_pointer_value().into(), val.into()], "snprintf").unwrap();
                                append_bounded(self, flt_ptr);
                            }
                        }
                    }
                }
                let strdup = self.get_or_declare_strdup();
                let dup = self.builder.build_call(strdup, &[buf_ptr.into()], "strdup").unwrap().try_as_basic_value().basic().unwrap();
                Ok(dup)
            }
            ExprKind::Closure { params, body, span: _ } => {
                let id = self.closure_count;
                self.closure_count += 1;
                let name = format!("hella.closure.{}", id);
                let mut param_types: Vec<inkwell::types::BasicMetadataTypeEnum> = Vec::new();
                for p in params {
                    let ty: crate::sema::Ty = (&p.ty).into();
                    let sema_ty = self.resolve_ty_for_codegen(&ty);
                    if let Some(bt) = self.llvm_ty_for_sema(&sema_ty) { param_types.push(bt.into()); } else { param_types.push(self.context.i64_type().into()); }
                }
                let fn_ty = self.context.i64_type().fn_type(&param_types, false);
                let func = self.module.add_function(&name, fn_ty, None);
                let prev_fn = self.cur_fn;
                let prev_block = self.builder.get_insert_block();
                let entry = self.context.append_basic_block(func, "entry");
                self.builder.position_at_end(entry);
                self.cur_fn = Some(func);
                self.vars.push(std::collections::HashMap::new());
                self.own_slots.push(Vec::new());
                self.scope_dtors.push(Vec::new());
                for (i, p) in params.iter().enumerate() {
                    let llvm_ty = self.llvm_ty_for(&p.ty);
                    let alloca = self.create_entry_block_alloca(&p.name, llvm_ty);
                    let param_val = func.get_nth_param(i as u32).unwrap();
                    self.builder.build_store(alloca, param_val).unwrap();
                    self.vars.last_mut().unwrap().insert(p.name.clone(), (alloca, llvm_ty));
                    self.track_own_param(alloca, &p.ty);
                    self.track_dtor_slot(alloca, &p.ty, llvm_ty);
                }
                let ret_val = match body.as_ref() {
                    ClosureBody::Expr(e) => Some(self.codegen_expr(e)?),
                    ClosureBody::Block(b) => { let _ = self.codegen_block(b)?; None },
                };
                if let Some(v) = ret_val {
                    // Move-out: if the body is a bare param, ownership
                    // transfers to the caller — drop its slot without destroying.
                    if let ClosureBody::Expr(e) = body.as_ref() {
                        if let ExprKind::Ident(name) = &e.kind {
                            if let Some((ptr, _)) = self.lookup_var(name) {
                                if let Some(top) = self.own_slots.last_mut() {
                                    if let Some(pos) = top.iter().position(|(p, _)| *p == ptr) {
                                        top.remove(pos);
                                    }
                                }
                                if let Some(top) = self.scope_dtors.last_mut() {
                                    if let Some(pos) = top.iter().position(|(p, _)| *p == ptr) {
                                        top.remove(pos);
                                    }
                                }
                            }
                        }
                    }
                    if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                        self.emit_current_scope_owns();
                        self.emit_current_scope_dtors();
                    }
                    if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                        self.builder.build_return(Some(&v)).unwrap();
                    }
                } else if self.builder.get_insert_block().unwrap().get_terminator().is_none() {
                    self.emit_current_scope_owns();
                    self.emit_current_scope_dtors();
                    self.builder.build_return(Some(&self.context.i64_type().const_int(0,false))).unwrap();
                }
                self.own_slots.pop();
                self.scope_dtors.pop();
                self.vars.pop();
                self.cur_fn = prev_fn;
                if let Some(bb) = prev_block { self.builder.position_at_end(bb); }
                Ok(func.as_global_value().as_pointer_value().into())
            }
            ExprKind::Tuple(exprs) => {
                let vals: Vec<BasicValueEnum> = exprs.iter().map(|e| self.codegen_expr(e).unwrap()).collect();
                let tys: Vec<BasicTypeEnum> = vals.iter().map(|v| v.get_type()).collect();
                let struct_ty = self.context.struct_type(&tys, false);
                let mut agg: BasicValueEnum = struct_ty.get_undef().into();
                for (i, v) in vals.into_iter().enumerate() {
                    let tmp = self.builder.build_insert_value(agg.into_struct_value(), v, i as u32, "tuple.ins").unwrap();
                    agg = tmp.as_basic_value_enum();
                }
                Ok(agg)
            }
            ExprKind::ArrayLit(elems) => {
                // Array value sized to the literal length, element type from
                // the first element (elements coerced to it). Used for
                // non-decl positions; `VarDecl` with `FixedArray` type stores
                // per-element via GEP for exact width matching.
                if elems.is_empty() {
                    return Ok(self.context.i64_type().array_type(0).const_zero().into());
                }
                let first = self.codegen_expr(&elems[0])?;
                let elem_ty = first.get_type();
                let arr_ty = match elem_ty {
                    BasicTypeEnum::IntType(it) => it.array_type(elems.len() as u32).into(),
                    BasicTypeEnum::FloatType(ft) => ft.array_type(elems.len() as u32).into(),
                    BasicTypeEnum::PointerType(pt) => pt.array_type(elems.len() as u32).into(),
                    BasicTypeEnum::StructType(st) => st.array_type(elems.len() as u32).into(),
                    BasicTypeEnum::ArrayType(at) => at.array_type(elems.len() as u32).into(),
                    _ => self.context.i64_type().array_type(elems.len() as u32).into(),
                };
                let mut agg: BasicValueEnum = match arr_ty {
                    BasicTypeEnum::ArrayType(at) => at.get_undef().into(),
                    _ => unreachable!(),
                };
                let first_c = self.coerce_to_ty(first, elem_ty);
                let tmp = self.builder.build_insert_value(agg.into_array_value(), first_c, 0, "arr.0").unwrap();
                agg = tmp.as_basic_value_enum();
                for (i, e) in elems.iter().enumerate().skip(1) {
                    let v = self.codegen_expr(e)?;
                    let cv = self.coerce_to_ty(v, elem_ty);
                    let tmp = self.builder.build_insert_value(agg.into_array_value(), cv, i as u32, &format!("arr.{i}")).unwrap();
                    agg = tmp.as_basic_value_enum();
                }
                Ok(agg)
            }
            ExprKind::VecEmpty(_) => {
                // Empty vector value: zeroed i64-slot struct. Declarations
                // refine the buffer type via their `Vec` type; this fallback
                // covers non-declaration positions.
                let vec_st = self.vec_struct_ty(self.context.i64_type().into());
                Ok(vec_st.const_zero().into())
            }
            ExprKind::MapLit { entries, .. } => {
                // Map value (non-declaration positions): slots inferred from
                // the first entry's shape; keys/values inserted per entry.
                let (dk, dv) = if entries.is_empty() {
                    (
                        self.context.i64_type().into(),
                        self.context.i64_type().into(),
                    )
                } else {
                    (
                        self.lit_slot_ty(&entries[0].0, true),
                        self.lit_slot_ty(&entries[0].1, false),
                    )
                };
                let map_st = self.map_struct_ty(dk, dv);
                let mut agg: BasicValueEnum<'ctx> = map_st.const_zero().into();
                // Rebuild per entry via GEP on a temp alloca (insertvalue on
                // nested arrays is awkward); then load the finished struct.
                let tmp = self.create_entry_block_alloca("map.tmp", map_st.into());
                self.builder.build_store(tmp, agg).unwrap();
                let entries_owned = entries.clone();
                self.store_map_entries(tmp, map_st, dk, dv, &entries_owned)?;
                agg = self.builder.build_load(map_st.as_basic_type_enum(), tmp, "map.tmp.load").unwrap();
                Ok(agg)
            }
            ExprKind::Null => Ok(self.context.ptr_type(inkwell::AddressSpace::default()).const_null().into()),
            ExprKind::Super => {
                if let Some((ptr, ty)) = self.lookup_var("this") {
                    let loaded = self.builder.build_load(ty, ptr, "super").unwrap();
                    Ok(loaded)
                } else { Err(CodegenError{message: "`super` outside class".into(), span: expr.span}) }
            }
            ExprKind::Paren(inner) => self.codegen_expr(inner),
            // `await t` (Async-6): block-join the task, then copy out the
            // inline result. `task<T>` values are opaque ptr handles; the
            // result type comes from `task_vars` (decl-site tracking) or
            // infer_expr_ty.
            ExprKind::Await { task, span } => {
                let handle = self.codegen_expr(task)?;
                let handle_ptr = handle.into_pointer_value();
                // join
                let join = self.get_or_declare_task_join();
                self.builder.build_call(join, &[handle_ptr.into()], "async.join").unwrap();
                // result typing
                let t_ty = self.infer_expr_ty(task).unwrap_or(crate::sema::Ty::Any);
                match t_ty {
                    crate::sema::Ty::Task(inner) => match inner.as_ref() {
                        crate::sema::Ty::Void => {
                            // task<void>: join, free, yield no value (i64 0
                            // filler; callers treat `await` as an expr stmt).
                            let res_fn = self.get_or_declare_task_result();
                            let scratch = self.builder.build_alloca(self.context.i64_type(), "async.void.scratch").unwrap();
                            self.builder.build_call(res_fn, &[handle_ptr.into(), scratch.into(), self.context.i64_type().const_int(0, false).into()], "async.result").unwrap();
                            Ok(self.context.i64_type().const_int(0, false).into())
                        }
                        inner_t => {
                            let rt = self.llvm_ty_for_sema(inner_t).ok_or(CodegenError {
                                message: format!("`await` result type has no lowering: {inner_t}"),
                                span: *span,
                            })?;
                            let n = self.llvm_byte_size(rt);
                            let slot = self.builder.build_alloca(rt, "async.ret").unwrap();
                            let res_fn = self.get_or_declare_task_result();
                            self.builder.build_call(res_fn, &[handle_ptr.into(), slot.into(), self.context.i64_type().const_int(n, false).into()], "async.result").unwrap();
                            Ok(self.builder.build_load(rt, slot, "async.result.load").unwrap())
                        }
                    },
                    _ => Err(CodegenError {
                        message: "`await` requires a `task<T>` handle".into(),
                        span: *span,
                    }),
                }
            }
            // `spawn f(args)` (Async-6): identical to an async call — the
            // Call arm already spawns async callees; `spawn` is the explicit
            // form for readability inside `scope`.
            ExprKind::Spawn { task, .. } => self.codegen_expr(task),
            ExprKind::New { ty, args, .. } => {
                // Heap construction: `new Type(args)` -> `own Type` pair.
                let inner_name = match ty {
                    Type::Named(n, _) => n.rsplit("::").next().unwrap_or(n).to_string(),
                    Type::Generic(n, _, _) => n.rsplit("::").next().unwrap_or(n).to_string(),
                    _ => {
                        return Err(CodegenError{message: format!("`new` requires a named type"), span: expr.span});
                    }
                };
                let st = *self.struct_types.get(&inner_name).ok_or(CodegenError{message: format!("unknown type `{inner_name}` for `new`"), span: expr.span})?;
                let pair_ty = *self.pair_types.get(&inner_name).ok_or(CodegenError{message: format!("no pair type for `{inner_name}`"), span: expr.span})?;
                let size = self.struct_byte_size(st);
                let malloc = self.get_or_declare_malloc();
                let call = self.builder.build_call(malloc, &[size.into()], "new.malloc").unwrap();
                let raw = call.try_as_basic_value().basic().unwrap().into_pointer_value();
                let heap_ptr = self.builder.build_bit_cast(raw, st.ptr_type(inkwell::AddressSpace::default()), "new.cast").unwrap().into_pointer_value();
                // Initialize via constructor or direct field stores.
                if let Some(ctors) = self.class_constructors.get(&inner_name).cloned() {
                    // Pick ctor by arity, allowing omitted trailing defaults
                    // (same rule as `Type(args)` calls).
                    let mut chosen: Option<(inkwell::values::FunctionValue<'ctx>, TyInfo)> = None;
                    for (func, info) in &ctors {
                        let min = info.params.len().saturating_sub(info.param_defaults.iter().rev().take_while(|d| d.is_some()).count());
                        if args.len() + 1 >= min && args.len() + 1 <= info.params.len() { chosen = Some((*func, info.clone())); break; }
                    }
                    let (ctor_fn, info) = chosen.or_else(|| ctors.first().cloned()).ok_or(CodegenError{message: format!("no constructor for `{inner_name}`"), span: expr.span})?;
                    let mut arg_vals: Vec<inkwell::values::BasicMetadataValueEnum> = vec![heap_ptr.into()];
                    for (i, a) in args.iter().enumerate() {
                        let v = self.codegen_call_arg(a)?;
                        let v = self.box_arg_for_param(v, &info, i+1, a.span())?;
                        arg_vals.push(v.into());
                    }
                    // Fill omitted trailing defaults.
                    for idx in (args.len() + 1)..info.params.len() {
                        let v = self.codegen_default_for_param(&info, idx)?;
                        arg_vals.push(v.into());
                    }
                    self.builder.build_call(ctor_fn, &arg_vals, "new.ctor").unwrap();
                } else if let Some(field_map) = self.struct_fields.get(&inner_name).cloned() {
                    let field_count = field_map.len();
                    if field_count > 0 && args.len() == field_count {
                        let mut sorted: Vec<(String, u32)> = field_map.into_iter().collect();
                        sorted.sort_by_key(|(_, idx)| *idx);
                        for (i, a) in args.iter().enumerate() {
                            let v = self.codegen_call_arg(a)?;
                            let idx = sorted[i].1;
                            let field_ptr = self.builder.build_struct_gep(st, heap_ptr, idx, "new.field").unwrap();
                            self.builder.build_store(field_ptr, v).unwrap();
                        }
                    } else if !args.is_empty() {
                        for a in args { let _ = self.codegen_call_arg(a)?; }
                    }
                } else {
                    for a in args { let _ = self.codegen_call_arg(a)?; }
                }
                let tag = self.class_tags.get(&inner_name).cloned().unwrap_or(0);
                let mut pair_val: BasicValueEnum<'ctx> = pair_ty.get_undef().into();
                let data = heap_ptr.as_basic_value_enum();
                let tmp = self.builder.build_insert_value(pair_val.into_struct_value(), data, 0, "own.data").unwrap();
                pair_val = tmp.as_basic_value_enum();
                let tag_val = self.context.i64_type().const_int(tag, false);
                let tmp2 = self.builder.build_insert_value(pair_val.into_struct_value(), tag_val, 1, "own.tag").unwrap();
                pair_val = tmp2.as_basic_value_enum();
                Ok(pair_val)
            }
            _ => todo!("unhandled expr {:?}", expr.kind),
        }
    }

    /// Load payload word `idx` from an enum struct value, across both
    /// layouts (legacy `{tag, i64}` and wide `{tag, [N x i64]}`).
    fn enum_payload_word(
        &self,
        scrut_val: &BasicValueEnum<'ctx>,
        idx: u32,
        name: &str,
    ) -> Result<inkwell::values::IntValue<'ctx>, CodegenError> {
        let payload = self
            .builder
            .build_extract_value(scrut_val.into_struct_value(), 1, name)
            .unwrap();
        if payload.is_array_value() {
            Ok(self
                .builder
                .build_extract_value(payload.into_array_value(), idx, name)
                .unwrap()
                .into_int_value())
        } else if idx == 0 {
            Ok(payload.into_int_value())
        } else {
            Err(CodegenError {
                message: "enum payload position out of range".into(),
                span: Span::new(0, 0),
            })
        }
    }

    fn codegen_match(
        &mut self,
        m: &MatchExpr,
        span: Span,
    ) -> Result<BasicValueEnum<'ctx>, CodegenError> {
        // Evaluate scrutinee once
        let scrut_val = self.codegen_expr(&m.scrutinee)?;
        // Resolve the scrutinee's sema type once (sema already validated
        // enum/tuple patterns). `infer_expr_ty` is best-effort here: complex
        // scrutinees (calls, etc.) may not infer, so keep it optional and
        // report a proper `CodegenError` instead of panicking below.
        let scrut_ty_opt = self.infer_expr_ty(&m.scrutinee).ok();
        let scrut_enum_name: Option<String> = match &scrut_ty_opt {
            Some(crate::sema::Ty::Enum(n)) => Some(n.clone()),
            _ => None,
        };
        let func = self.cur_fn.unwrap();
        let merge_bb = self.context.append_basic_block(func, "match.merge");
        // Determine result type lazily via first arm body; allocate after
        let mut result_alloc: Option<(
            PointerValue<'ctx>,
            BasicTypeEnum<'ctx>,
        )> = None;
        // start check block is current insertion block after scrutinee
        let mut cur_check_bb = self.builder.get_insert_block().unwrap();
        for (idx, arm) in m.arms.iter().enumerate() {
            let is_last = idx == m.arms.len() - 1;
            let arm_bb = self
                .context
                .append_basic_block(func, &format!("match.arm{}", idx));
            let next_bb = if is_last {
                merge_bb
            } else {
                self.context
                    .append_basic_block(func, &format!("match.next{}", idx))
            };
            // Emit pattern check in cur_check_bb
            self.builder.position_at_end(cur_check_bb);
            // pattern match value (i1)
            let pattern_is_wild = match &arm.pattern {
                Pattern::Wildcard(_) | Pattern::Var(_, _) => true,
                Pattern::Alternative(pats, _) => pats.iter().any(|p| matches!(p, Pattern::Wildcard(_) | Pattern::Var(_, _))),
                _ => false,
            };
            let pattern_val: inkwell::values::IntValue<'ctx> =
                if pattern_is_wild {
                    self.context.bool_type().const_int(1, false)
                } else {
                    match &arm.pattern {
                        Pattern::LitInt(v, _) => {
                            let lit = self
                                .context
                                .i64_type()
                                .const_int(*v as u64, true);
                            self.builder
                                .build_int_compare(
                                    IntPredicate::EQ,
                                    scrut_val.into_int_value(),
                                    lit,
                                    "match.pat",
                                )
                                .unwrap()
                        }
                        Pattern::LitBool(b, _) => {
                            let lit = self
                                .context
                                .bool_type()
                                .const_int(if *b { 1 } else { 0 }, false);
                            self.builder
                                .build_int_compare(
                                    IntPredicate::EQ,
                                    scrut_val.into_int_value(),
                                    lit,
                                    "match.pat",
                                )
                                .unwrap()
                        }
                        Pattern::Wildcard(_) => unreachable!(),
                        Pattern::Var(_, _) => unreachable!(),
                        Pattern::Alternative(pats, _) => {
                            // `a | b` or `a or b` : OR of each alternative's check
                            let mut or_val: Option<inkwell::values::IntValue<'ctx>> = None;
                            for pat in pats {
                                let check = match pat {
                                    Pattern::LitInt(v, _) => {
                                        let lit = self.context.i64_type().const_int(*v as u64, true);
                                        self.builder.build_int_compare(IntPredicate::EQ, scrut_val.into_int_value(), lit, "match.alt").unwrap()
                                    }
                                    Pattern::LitBool(b, _) => {
                                        let lit = self.context.bool_type().const_int(if *b {1} else {0}, false);
                                        self.builder.build_int_compare(IntPredicate::EQ, scrut_val.into_int_value(), lit, "match.alt").unwrap()
                                    }
                                    Pattern::Wildcard(_) | Pattern::Var(_, _) => self.context.bool_type().const_int(1, false),
                                    Pattern::Enum{variant, payload, ..} => {
                                        let ename = scrut_enum_name.clone().ok_or(CodegenError{message: format!("enum pattern `{variant}` on non-enum scrutinee"), span: pat.span()})?;
                                        let tag_map = self.enum_variant_tags.get(&ename).ok_or(CodegenError{message: format!("unknown enum `{ename}`"), span: pat.span()})?;
                                        let tag = *tag_map.get(variant.rsplit("::").next().unwrap_or(variant)).ok_or(CodegenError{message: format!("unknown variant `{variant}` for enum `{ename}`"), span: pat.span()})? as u64;
                                        let tag_lit = self.context.i32_type().const_int(tag, false);
                                        let enum_tag = self.builder.build_extract_value(scrut_val.into_struct_value(), 0, "enum.tag.alt").unwrap().into_int_value();
                                        let tag_eq = self.builder.build_int_compare(IntPredicate::EQ, enum_tag, tag_lit, "match.enum.tag.alt").unwrap();
                                        if let Some(p) = payload {
                                            if p.len() == 1 {
                                                let payload_val = self.builder.build_extract_value(scrut_val.into_struct_value(), 1, "enum.payload.alt").unwrap();
                                                match &p[0] {
                                                    Pattern::LitInt(v2, _) => {
                                                        let lit2 = self.context.i64_type().const_int(*v2 as u64, true);
                                                        let inner = self.builder.build_int_compare(IntPredicate::EQ, payload_val.into_int_value(), lit2, "match.alt.payload").unwrap();
                                                        self.builder.build_and(tag_eq, inner, "match.alt.and").unwrap()
                                                    }
                                                    _ => tag_eq,
                                                }
                                            } else if p.len() > 1 {
                                                // Wide payload: check each literal position.
                                                // Positions address WORDS, so track a cursor
                                                // (fields may span several words).
                                                let aftys = self.enum_payload_tys.get(&ename).and_then(|m| m.get(variant.rsplit("::").next().unwrap_or(variant))).cloned().unwrap_or_default();
                                                let mut cursor = 0u32;
                                                let mut and_val = tag_eq;
                                                for (pi, sub) in p.iter().enumerate() {
                                                    let k = aftys.get(pi).and_then(|t| Self::llvm_word_count(t)).unwrap_or(1).max(1) as u32;
                                                    if let Pattern::LitInt(v2, _) = sub {
                                                        let w = self.enum_payload_word(&scrut_val, cursor, "match.alt.payload")?;
                                                        let lit2 = self.context.i64_type().const_int(*v2 as u64, true);
                                                        let inner = self.builder.build_int_compare(IntPredicate::EQ, w, lit2, "match.alt.payload.eq").unwrap();
                                                        and_val = self.builder.build_and(and_val, inner, "match.alt.and").unwrap();
                                                    } else if let Pattern::LitBool(b2, _) = sub {
                                                        let w = self.enum_payload_word(&scrut_val, cursor, "match.alt.payload")?;
                                                        let lit2 = self.context.bool_type().const_int(if *b2 { 1 } else { 0 }, false);
                                                        let wb = self.builder.build_int_truncate(w, self.context.bool_type(), "match.alt.trunc").unwrap();
                                                        let inner = self.builder.build_int_compare(IntPredicate::EQ, wb, lit2, "match.alt.payload.eq").unwrap();
                                                        and_val = self.builder.build_and(and_val, inner, "match.alt.and").unwrap();
                                                    }
                                                    cursor += k;
                                                }
                                                and_val
                                            } else { tag_eq }
                                        } else { tag_eq }
                                    }
                                    Pattern::Tuple(subs, _) => {
                                        // For tuple alternative like `(1,2) | (3,4)`, check each tuple
                                        if let Some(crate::sema::Ty::Tuple(tys)) = scrut_ty_opt.clone() {
                                            if tys.len() == subs.len() {
                                                let mut and_val: Option<inkwell::values::IntValue<'ctx>> = None;
                                                for (i, subpat) in subs.iter().enumerate() {
                                                    let elem_val = self.builder.build_extract_value(scrut_val.into_struct_value(), i as u32, "tuple.alt.elem").unwrap();
                                                    let elem_check = match subpat {
                                                        Pattern::Wildcard(_) | Pattern::Var(_, _) => self.context.bool_type().const_int(1, false),
                                                        Pattern::LitInt(v2, _) => {
                                                            let lit2 = self.context.i64_type().const_int(*v2 as u64, true);
                                                            self.builder.build_int_compare(IntPredicate::EQ, elem_val.into_int_value(), lit2, "tuple.alt.lit").unwrap()
                                                        }
                                                        Pattern::LitBool(b2, _) => {
                                                            let lit2 = self.context.bool_type().const_int(if *b2 {1} else {0}, false);
                                                            self.builder.build_int_compare(IntPredicate::EQ, elem_val.into_int_value(), lit2, "tuple.alt.lit").unwrap()
                                                        }
                                                        _ => self.context.bool_type().const_int(1, false),
                                                    };
                                                    and_val = Some(match and_val {
                                                        Some(prev) => self.builder.build_and(prev, elem_check, "tuple.alt.and").unwrap(),
                                                        None => elem_check,
                                                    });
                                                }
                                                and_val.unwrap_or_else(|| self.context.bool_type().const_int(1, false))
                                            } else { self.context.bool_type().const_int(0, false) }
                                        } else { self.context.bool_type().const_int(0, false) }
                                    }
                                    Pattern::Alternative(_, _) => self.context.bool_type().const_int(1, false),
                                };
                                or_val = Some(match or_val {
                                    Some(prev) => self.builder.build_or(prev, check, "match.alt.or").unwrap(),
                                    None => check,
                                });
                            }
                            or_val.unwrap_or_else(|| self.context.bool_type().const_int(0, false))
                        }
                        Pattern::Tuple(pats, _) => {
                            // `(a, b)` where scrutinee is tuple: AND of each element's check
                            let mut and_val: Option<inkwell::values::IntValue<'ctx>> = None;
                            for (i, pat) in pats.iter().enumerate() {
                                let elem_val = self.builder.build_extract_value(scrut_val.into_struct_value(), i as u32, "tuple.elem").unwrap();
                                let elem_check = match pat {
                                    Pattern::Wildcard(_) | Pattern::Var(_, _) => self.context.bool_type().const_int(1, false),
                                    Pattern::LitInt(v, _) => {
                                        let lit = self.context.i64_type().const_int(*v as u64, true);
                                        self.builder.build_int_compare(IntPredicate::EQ, elem_val.into_int_value(), lit, "tuple.pat").unwrap()
                                    }
                                    Pattern::LitBool(b, _) => {
                                        let lit = self.context.bool_type().const_int(if *b {1} else {0}, false);
                                        self.builder.build_int_compare(IntPredicate::EQ, elem_val.into_int_value(), lit, "tuple.pat").unwrap()
                                    }
                                    Pattern::Enum{..} => {
                                        // Tuple element is itself an enum value: binding only,
                                        // payload checks happen when that element is matched.
                                        self.context.bool_type().const_int(1, false)
                                    }
                                    Pattern::Tuple(_, _) => self.context.bool_type().const_int(1, false),
                                    Pattern::Alternative(alts, _) => {
                                        let mut or2: Option<inkwell::values::IntValue<'ctx>> = None;
                                        for alt in alts {
                                            let alt_check = match alt {
                                                Pattern::LitInt(v2, _) => {
                                                    let lit2 = self.context.i64_type().const_int(*v2 as u64, true);
                                                    self.builder.build_int_compare(IntPredicate::EQ, elem_val.into_int_value(), lit2, "tuple.alt").unwrap()
                                                }
                                                _ => self.context.bool_type().const_int(1, false),
                                            };
                                            or2 = Some(match or2 {
                                                Some(prev) => self.builder.build_or(prev, alt_check, "tuple.alt.or").unwrap(),
                                                None => alt_check,
                                            });
                                        }
                                        or2.unwrap_or_else(|| self.context.bool_type().const_int(0, false))
                                    }
                                };
                                and_val = Some(match and_val {
                                    Some(prev) => self.builder.build_and(prev, elem_check, "tuple.and").unwrap(),
                                    None => elem_check,
                                });
                            }
                            and_val.unwrap_or_else(|| self.context.bool_type().const_int(1, false))
                        }
                        Pattern::Enum{variant, payload, ..} => {
                            let ename = scrut_enum_name.clone().ok_or(CodegenError{message: format!("enum pattern `{variant}` on non-enum scrutinee"), span: arm.pattern.span()})?;
                            let tag_map = self.enum_variant_tags.get(&ename).ok_or(CodegenError{message: format!("unknown enum `{ename}`"), span: arm.pattern.span()})?;
                            let tag = *tag_map.get(variant.rsplit("::").next().unwrap_or(variant)).ok_or(CodegenError{message: format!("unknown variant `{variant}` for enum `{ename}`"), span: arm.pattern.span()})? as u64;
                            let tag_lit = self.context.i32_type().const_int(tag, false);
                            let enum_tag = self.builder.build_extract_value(scrut_val.into_struct_value(), 0, "enum.tag").unwrap().into_int_value();
                            let tag_eq = self.builder.build_int_compare(IntPredicate::EQ, enum_tag, tag_lit, "match.enum.tag").unwrap();
                            if let Some(pats) = payload.clone() {
                                let payload_val = self.builder.build_extract_value(scrut_val.into_struct_value(), 1, "enum.payload").unwrap();
                                let inner_eq = if pats.len() == 1 {
                                    match &pats[0] {
                                        Pattern::Wildcard(_) => self.context.bool_type().const_int(1, false),
                                        Pattern::LitInt(v, _) => {
                                            let lit = self.context.i64_type().const_int(*v as u64, true);
                                            self.builder.build_int_compare(IntPredicate::EQ, payload_val.into_int_value(), lit, "match.enum.payload").unwrap()
                                        }
                                        Pattern::LitBool(b, _) => {
                                            let lit = self.context.bool_type().const_int(if *b {1} else {0}, false);
                                            self.builder.build_int_compare(IntPredicate::EQ, payload_val.into_int_value(), lit, "match.enum.payload").unwrap()
                                        }
                                        Pattern::Tuple(_, _) => {
                                            // Enum payload is a tuple like `MyVariant((a,b))`;
                                            // element-wise checks bind in the arm body.
                                            self.context.bool_type().const_int(1, false)
                                        }
                                        _ => self.context.bool_type().const_int(1, false),
                                    }
                                } else if pats.len() > 1 {
                                    // Wide payload: check each literal position
                                    // (tag is ANDed by the caller below).
                                    // Positions address WORDS: track a cursor
                                    // since fields may span several words.
                                    let mftys = self.enum_payload_tys.get(&ename).and_then(|m| m.get(variant.rsplit("::").next().unwrap_or(variant))).cloned().unwrap_or_default();
                                    let mut cursor = 0u32;
                                    let mut and_val = self.context.bool_type().const_int(1, false);
                                    for (pi, sub) in pats.iter().enumerate() {
                                        let k = mftys.get(pi).and_then(|t| Self::llvm_word_count(t)).unwrap_or(1).max(1) as u32;
                                        let check = match sub {
                                            Pattern::Wildcard(_) | Pattern::Var(_, _) => self.context.bool_type().const_int(1, false),
                                            Pattern::LitInt(v2, _) => {
                                                let w = self.enum_payload_word(&scrut_val, cursor, "match.enum.payload")?;
                                                let lit2 = self.context.i64_type().const_int(*v2 as u64, true);
                                                self.builder.build_int_compare(IntPredicate::EQ, w, lit2, "match.enum.payload.eq").unwrap()
                                            }
                                            Pattern::LitBool(b2, _) => {
                                                let w = self.enum_payload_word(&scrut_val, cursor, "match.enum.payload")?;
                                                let lit2 = self.context.bool_type().const_int(if *b2 { 1 } else { 0 }, false);
                                                let wb = self.builder.build_int_truncate(w, self.context.bool_type(), "match.enum.trunc").unwrap();
                                                self.builder.build_int_compare(IntPredicate::EQ, wb, lit2, "match.enum.payload.eq").unwrap()
                                            }
                                            _ => self.context.bool_type().const_int(1, false),
                                        };
                                        and_val = self.builder.build_and(and_val, check, "match.enum.and").unwrap();
                                        cursor += k;
                                    }
                                    and_val
                                } else {
                                    self.context.bool_type().const_int(1, false)
                                };
                                self.builder.build_and(tag_eq, inner_eq, "match.enum.and").unwrap()
                            } else {
                                tag_eq
                            }
                        }
                    }
                };
            // Handle guard
            if let Some(guard_expr) = &arm.guard {
                // pattern matched -> check guard, else go next
                let guard_check_bb = self
                    .context
                    .append_basic_block(func, &format!("match.guard{}", idx));
                self.builder
                    .build_conditional_branch(
                        pattern_val,
                        guard_check_bb,
                        next_bb,
                    )
                    .unwrap();
                self.builder.position_at_end(guard_check_bb);
                let guard_val = self.codegen_expr(guard_expr)?;
                // guard must be bool
                self.builder
                    .build_conditional_branch(
                        guard_val.into_int_value(),
                        arm_bb,
                        next_bb,
                    )
                    .unwrap();
            } else {
                self.builder
                    .build_conditional_branch(pattern_val, arm_bb, next_bb)
                    .unwrap();
            }
            // Emit arm body - bind pattern vars in arm scope
            self.builder.position_at_end(arm_bb);
            self.vars.push(HashMap::new());
            // Bind pattern variables: Var, Tuple, Enum payload, Alternative
            match &arm.pattern {
                Pattern::Var(name, _) => {
                    let ty = scrut_val.get_type();
                    let alloc = self.create_entry_block_alloca(name, ty);
                    self.builder.build_store(alloc, scrut_val).unwrap();
                    self.vars.last_mut().unwrap().insert(name.clone(), (alloc, ty));
                }
                Pattern::Tuple(pats, _) => {
                    for (i, pat) in pats.iter().enumerate() {
                        if let Pattern::Var(vname, _) = pat {
                            let elem_val = self.builder.build_extract_value(scrut_val.into_struct_value(), i as u32, "tuple.bind").unwrap();
                            let ty = elem_val.get_type();
                            let alloc = self.create_entry_block_alloca(vname, ty);
                            self.builder.build_store(alloc, elem_val).unwrap();
                            self.vars.last_mut().unwrap().insert(vname.clone(), (alloc, ty));
                        } else if let Pattern::Tuple(inner, _) = pat {
                            // Nested tuple like `((a,b), c)` - handle one level
                            let elem_val = self.builder.build_extract_value(scrut_val.into_struct_value(), i as u32, "tuple.nested").unwrap();
                            for (j, ipat) in inner.iter().enumerate() {
                                if let Pattern::Var(n2, _) = ipat {
                                    let inner_val = self.builder.build_extract_value(elem_val.into_struct_value(), j as u32, "tuple.inner.bind").unwrap();
                                    let ty2 = inner_val.get_type();
                                    let alloc2 = self.create_entry_block_alloca(n2, ty2);
                                    self.builder.build_store(alloc2, inner_val).unwrap();
                                    self.vars.last_mut().unwrap().insert(n2.clone(), (alloc2, ty2));
                                }
                            }
                        }
                    }
                }
                Pattern::Alternative(pats, _) => {
                    // `a | b` or `a or b` where `a`/`b` are `Var` or literals: bind first Var if any
                    for pat in pats {
                        if let Pattern::Var(name, _) = pat {
                            let ty = scrut_val.get_type();
                            let alloc = self.create_entry_block_alloca(name, ty);
                            self.builder.build_store(alloc, scrut_val).unwrap();
                            self.vars.last_mut().unwrap().insert(name.clone(), (alloc, ty));
                            break;
                        } else if let Pattern::Tuple(subs, _) = pat {
                            for (i, spat) in subs.iter().enumerate() {
                                if let Pattern::Var(n, _) = spat {
                                    let elem_val = self.builder.build_extract_value(scrut_val.into_struct_value(), i as u32, "alt.tuple.bind").unwrap();
                                    let ty = elem_val.get_type();
                                    let alloc = self.create_entry_block_alloca(n, ty);
                                    self.builder.build_store(alloc, elem_val).unwrap();
                                    self.vars.last_mut().unwrap().insert(n.clone(), (alloc, ty));
                                }
                            }
                            break;
                        }
                    }
                }
                Pattern::Enum{ payload: Some(pats), variant, ..} => {
                    if pats.len() == 1 {
                        match &pats[0] {
                            Pattern::Var(vname, _) => {
                                let payload_val = self.builder.build_extract_value(scrut_val.into_struct_value(), 1, "enum.payload.bind").unwrap();
                                let ty = payload_val.get_type();
                                let alloc = self.create_entry_block_alloca(vname, ty);
                                self.builder.build_store(alloc, payload_val).unwrap();
                                self.vars.last_mut().unwrap().insert(vname.clone(), (alloc, ty));
                            }
                            Pattern::Tuple(subs, _) => {
                                let payload_val = self.builder.build_extract_value(scrut_val.into_struct_value(), 1, "enum.payload.tuple").unwrap();
                                for (i, spat) in subs.iter().enumerate() {
                                    if let Pattern::Var(n, _) = spat {
                                        let elem_val = self.builder.build_extract_value(payload_val.into_struct_value(), i as u32, "enum.tuple.bind").unwrap();
                                        let ty = elem_val.get_type();
                                        let alloc = self.create_entry_block_alloca(n, ty);
                                        self.builder.build_store(alloc, elem_val).unwrap();
                                        self.vars.last_mut().unwrap().insert(n.clone(), (alloc, ty));
                                    }
                                }
                            }
                            _ => {}
                        }
                    } else {
                        // Wide payload: reassemble each bound position from
                        // its words via the variant's field types.
                        let vbase: &str = variant.rsplit("::").next().unwrap_or(variant);
                        let ename = scrut_enum_name.clone().unwrap_or_default();
                        let ftys = self.enum_payload_tys.get(&ename).and_then(|m| m.get(vbase)).cloned().unwrap_or_default();
                        let ctx: &'ctx Context = self.context;
                        let i64_ty = ctx.i64_type();
                        let mut cursor = 0u32;
                        for (i, pat) in pats.iter().enumerate() {
                            let fty: BasicTypeEnum<'ctx> = ftys.get(i).cloned().unwrap_or(i64_ty.into());
                            let k = Self::llvm_word_count(&fty).unwrap_or(1).max(1) as u32;
                            if let Pattern::Var(vname, _) = pat {
                                let tmp_words = self.create_entry_block_alloca("enum.bind.words", i64_ty.array_type(k).into());
                                for w in 0..k {
                                    let word = self.enum_payload_word(&scrut_val, cursor + w, "enum.bind")?;
                                    let slot = unsafe { self.builder.build_gep(i64_ty, tmp_words, &[i64_ty.const_int(w as u64, false)], "enum.bind.slot").unwrap() };
                                    self.builder.build_store(slot, word).unwrap();
                                }
                                let opaque_ptr2: BasicTypeEnum<'ctx> = ctx.ptr_type(inkwell::AddressSpace::default()).into();
                                let field_ptr = self.builder.build_bit_cast(tmp_words.as_basic_value_enum(), opaque_ptr2, "enum.bind.cast").unwrap().into_pointer_value();
                                let val = self.builder.build_load(fty, field_ptr, "enum.bind.val").unwrap();
                                let alloc = self.create_entry_block_alloca(vname, fty);
                                self.builder.build_store(alloc, val).unwrap();
                                self.vars.last_mut().unwrap().insert(vname.clone(), (alloc, fty));
                            }
                            cursor += k;
                        }
                    }
                }
                _ => {}
            }
            let body_val_opt: Option<BasicValueEnum<'ctx>> = match &arm.body {
                MatchArmBody::Expr(e) => Some(self.codegen_expr(e)?),
                MatchArmBody::Block(b) => {
                    let _ = self.codegen_block(b)?;
                    None
                }
            };
            self.vars.pop();
            if let Some(v) = body_val_opt {
                if result_alloc.is_none() {
                    let ty = v.get_type();
                    // allocate in entry for result
                    let alloc =
                        self.create_entry_block_alloca("match.result", ty);
                    result_alloc = Some((alloc, ty));
                }
                let (ptr, _) = result_alloc.unwrap();
                // Need to handle if current block already terminated (e.g., return inside arm)
                if self
                    .builder
                    .get_insert_block()
                    .unwrap()
                    .get_terminator()
                    .is_none()
                {
                    self.builder.build_store(ptr, v).unwrap();
                }
            }
            if self
                .builder
                .get_insert_block()
                .unwrap()
                .get_terminator()
                .is_none()
            {
                self.builder.build_unconditional_branch(merge_bb).unwrap();
            }
            cur_check_bb = next_bb;
            // if last arm was wildcard and had no next, cur_check_bb is merge; no need to continue
            if is_last {
                break;
            }
        }
        // Position at merge for subsequent code
        self.builder.position_at_end(merge_bb);
        if let Some((ptr, ty)) = result_alloc {
            let loaded =
                self.builder.build_load(ty, ptr, "match.result").unwrap();
            Ok(loaded)
        } else {
            // void match (all arms blocks) — return dummy int 0 for expression context; caller in ExprStmt will discard
            Ok(self.context.i64_type().const_int(0, false).into())
        }
    }

    // Helper: compute GEP pointer to field `field` of object expression.
    // The object struct is resolved with per-level `own` auto-deref (see
    // `obj_struct_ptr`), so chains like `o.pet.name` read through heap
    // pairs instead of their storage bytes.
    fn codegen_field_ptr(
        &self,
        object: &Expr,
        field: &str,
    ) -> Result<PointerValue<'ctx>, CodegenError> {
        let (obj_ptr, obj_name) = self.obj_struct_ptr(object)?;
        let fields = self.struct_fields.get(&obj_name).ok_or(CodegenError {
            message: format!("unknown struct {obj_name}"),
            span: object.span,
        })?;
        let field_idx = *fields.get(field).ok_or(CodegenError {
            message: format!("struct {obj_name} has no field {field}"),
            span: object.span,
        })?;
        let st = self.struct_types.get(&obj_name).unwrap();
        Ok(self
            .builder
            .build_struct_gep(*st, obj_ptr, field_idx, field)
            .unwrap())
    }

    /// Pointer to the struct instance denoted by `object`, plus its struct
    /// name. `own` pairs auto-deref at every level: a direct `own` variable
    /// yields its heap data pointer, and an `own` field in a chain is
    /// loaded and unwrapped before descending further.
    fn obj_struct_ptr(
        &self,
        object: &Expr,
    ) -> Result<(PointerValue<'ctx>, String), CodegenError> {
        match &object.kind {
            ExprKind::Ident(name) => {
                let lookup = name.rsplit("::").next().unwrap_or(name);
                let (ptr, ty) = self.lookup_var(name).or_else(|| self.lookup_var(lookup)).ok_or(CodegenError{message: format!("undefined var {name}"), span: object.span})?;
                if ty.is_struct_type() {
                    let st = ty.into_struct_type();
                    if let Some(inner) = self.pair_owner_of(st) {
                        let pair_val = self.builder.build_load(ty, ptr, "own.load").unwrap();
                        let data = self.builder.build_extract_value(pair_val.into_struct_value(), 0, "own.data").unwrap().into_pointer_value();
                        return Ok((data, inner));
                    }
                }
                let sname = self.ty_to_struct_name(&ty)?;
                Ok((ptr, sname))
            }
            ExprKind::This => {
                let (ptr, ty) = self.lookup_var("this").ok_or(CodegenError{message: "`this` outside method".into(), span: object.span})?;
                let sname = self.cur_class.clone().ok_or(CodegenError{message: "`this` outside method".into(), span: object.span})?;
                let instance_ptr = self.builder.build_load(ty, ptr, "this.load").unwrap().into_pointer_value();
                Ok((instance_ptr, sname))
            }
            ExprKind::Super => {
                let (ptr, ty) = self.lookup_var("this").ok_or(CodegenError{message: "`super` outside method".into(), span: object.span})?;
                let sname = self.cur_class.clone().ok_or(CodegenError{message: "`super` outside method".into(), span: object.span})?;
                let instance_ptr = self.builder.build_load(ty, ptr, "this.load").unwrap().into_pointer_value();
                Ok((instance_ptr, sname))
            }
            ExprKind::MemberAccess {
                object: inner,
                field: inner_field,
                ..
            } => {
                // Storage slot of `inner.inner_field`, then deref when the
                // field itself is an `own` pair.
                let (obj_ptr, obj_name) = self.obj_struct_ptr(inner)?;
                let fields = self.struct_fields.get(&obj_name).ok_or(CodegenError {
                    message: format!("unknown struct {obj_name}"),
                    span: object.span,
                })?;
                let field_idx = *fields.get(inner_field.as_str()).ok_or(CodegenError {
                    message: format!("struct {obj_name} has no field {inner_field}"),
                    span: object.span,
                })?;
                let st = self.struct_types.get(&obj_name).unwrap();
                let slot = self
                    .builder
                    .build_struct_gep(*st, obj_ptr, field_idx, inner_field)
                    .unwrap();
                let fty = st.get_field_type_at_index(field_idx).unwrap();
                if fty.is_struct_type() {
                    let fst = fty.into_struct_type();
                    if let Some(inner_owner) = self.pair_owner_of(fst) {
                        let pair_val = self.builder.build_load(fty, slot, "own.load").unwrap();
                        let data = self.builder.build_extract_value(pair_val.into_struct_value(), 0, "own.data").unwrap().into_pointer_value();
                        return Ok((data, inner_owner));
                    }
                }
                let sname = self.ty_to_struct_name(&fty)?;
                Ok((slot, sname))
            }
            _ => Err(CodegenError {
                message: "field access base must be variable or field".into(),
                span: object.span,
            }),
        }
    }

    fn codegen_as_ptr(&self, object: &Expr) -> Result<PointerValue<'ctx>, CodegenError> {
        match &object.kind {
            ExprKind::Ident(name) => {
                let (ptr, _) = self.lookup_var(name).ok_or(CodegenError{message: format!("undefined var {name}"), span: object.span})?;
                Ok(ptr)
            },
            ExprKind::This => {
                let (ptr, ty) = self.lookup_var("this").ok_or(CodegenError{message: "`this` outside method".into(), span: object.span})?;
                let loaded = self.builder.build_load(ty, ptr, "this.load").unwrap().into_pointer_value();
                Ok(loaded)
            },
            ExprKind::MemberAccess{object: inner, field, ..} => {
                // `a.b` storage address (for `ref` args, setters, field
                // stores — never deref'd here).
                Ok(self.codegen_field_ptr(inner, field)?)
            },
            _ => Err(CodegenError{message: "cannot take pointer of expression for property/method".into(), span: object.span}),
        }
    }

    fn ty_to_struct_name(
        &self,
        ty: &BasicTypeEnum<'ctx>,
    ) -> Result<String, CodegenError> {
        if let BasicTypeEnum::StructType(st) = ty {
            for (name, s) in &self.struct_types {
                if *s == *st {
                    return Ok(name.clone());
                }
                if s.as_basic_type_enum() == *ty {
                    return Ok(name.clone());
                }
            }
            for (name, e) in &self.enum_types {
                if *e == *st {
                    return Ok(name.clone());
                }
                if e.as_basic_type_enum() == *ty {
                    return Ok(name.clone());
                }
            }
            Err(CodegenError {
                message: "struct type not found".into(),
                span: Span::new(0, 0),
            })
        } else {
            Err(CodegenError {
                message: "variable is not struct".into(),
                span: Span::new(0, 0),
            })
        }
    }

    /// Map an LLVM field type back to a sema type (int widths, structs
    /// including trait pairs).
    fn llvm_field_to_sema(
        &self,
        fty: BasicTypeEnum<'ctx>,
        span: Span,
    ) -> Result<crate::sema::Ty, CodegenError> {
        if fty.is_int_type() {
            let bw = fty.into_int_type().get_bit_width();
            if bw == 1 {
                Ok(crate::sema::Ty::Bool)
            } else {
                Ok(crate::sema::Ty::Int)
            }
        } else if fty.is_struct_type() {
            let st = fty.into_struct_type();
            if let Some(tn) = self.pair_owner_of(st) {
                return Ok(crate::sema::Ty::Struct(tn));
            }
            let sname2 = self.ty_to_struct_name(&fty)?;
            Ok(crate::sema::Ty::Struct(sname2))
        } else {
            Err(CodegenError {
                message: "unsupported field type inference".into(),
                span,
            })
        }
    }

    /// Field type through a trait-typed receiver: from the first
    /// implementor (sema validated agreement across all of them).
    fn infer_trait_field(
        &self,
        tname: &str,
        field: &str,
        span: Span,
    ) -> Result<crate::sema::Ty, CodegenError> {
        let impls = self.implementors_of(tname);
        let first = impls.first().ok_or(CodegenError {
            message: format!("trait `{tname}` has no implementors"),
            span,
        })?;
        let fields = self.struct_fields.get(first).ok_or(CodegenError {
            message: format!("unknown class `{first}`"),
            span,
        })?;
        let idx = fields.get(field).ok_or(CodegenError {
            message: format!("trait `{tname}` has no field `{field}`"),
            span,
        })?;
        let st = self.struct_types.get(first).ok_or(CodegenError {
            message: format!("unknown class `{first}`"),
            span,
        })?;
        let fty = st.get_field_type_at_index(*idx).unwrap();
        self.llvm_field_to_sema(fty, span)
    }

    fn infer_expr_ty(
        &self,
        expr: &Expr,
    ) -> Result<crate::sema::Ty, CodegenError> {
        match &expr.kind {
            ExprKind::Ident(name) => {
                let lookup = name.rsplit("::").next().unwrap_or(name);
                // `task<T>` (Async-6): handles lower to an opaque ptr, so
                // the decl-site registration must win over the generic
                // pointer decode below.
                if let Some(t) = self.task_vars.get(name).or_else(|| self.task_vars.get(lookup)) {
                    return Ok(t.clone());
                }
                // Vectors lower as anonymous structs; report the vec type
                // instead of attempting struct-name resolution (which would
                // fail to find them in `struct_types`).
                if self.is_vec_var(name) || (lookup != name && self.is_vec_var(lookup)) {
                    return Ok(crate::sema::Ty::Vec(Box::new(crate::sema::Ty::Any)));
                }
                // Same for maps (anonymous `{ keys, vals, len }` structs).
                if self.is_map_var(name) || (lookup != name && self.is_map_var(lookup)) {
                    return Ok(crate::sema::Ty::Map {
                        key: Box::new(crate::sema::Ty::Any),
                        value: Box::new(crate::sema::Ty::Any),
                    });
                }
                for scope in self.vars.iter().rev() {
                    if let Some((_, ty)) = scope.get(name).or_else(|| scope.get(lookup)) {
                        if ty.is_struct_type() {
                            // Trait-object pair: report the trait name so
                            // member/method resolution takes the trait path.
                            if let Some(tn) = self.pair_owner_of(ty.into_struct_type()) {
                                return Ok(crate::sema::Ty::Struct(tn));
                            }
                            if let Ok(sname) = self.ty_to_struct_name(ty) {
                                if self.enum_types.contains_key(&sname) {
                                    return Ok(crate::sema::Ty::Enum(sname));
                                }
                                return Ok(crate::sema::Ty::Struct(sname));
                            }
                            // Anonymous struct (tuple literal): decode fields back to sema types.
                            let st = ty.into_struct_type();
                            let mut tys = Vec::new();
                            let mut ok = true;
                            for i in 0..st.count_fields() {
                                match st.get_field_type_at_index(i) {
                                    Some(fty) => match self.llvm_field_to_sema(fty, expr.span) {
                                        Ok(t) => tys.push(t),
                                        Err(_) => { ok = false; break; }
                                    },
                                    None => { ok = false; break; }
                                }
                            }
                            if ok && !tys.is_empty() {
                                return Ok(crate::sema::Ty::Tuple(tys));
                            }
                            return Err(CodegenError{message: format!("cannot infer type of {name}"), span: expr.span});
                        } else if ty.is_int_type() {
                            let bw = ty.into_int_type().get_bit_width();
                            if bw == 1 { return Ok(crate::sema::Ty::Bool); } else { return Ok(crate::sema::Ty::Int); }
                        } else if ty.is_pointer_type() {
                            return Ok(crate::sema::Ty::Pointer(Box::new(crate::sema::Ty::Int)));
                        } else if ty.is_array_type() {
                            return Ok(crate::sema::Ty::Array(Box::new(crate::sema::Ty::Int)));
                        }
                    }
                }
                for (gname, (_, gty)) in &self.globals.clone() {
                    if gname == name || gname == lookup {
                        // Globals resolve like locals for member/method
                        // dispatch (pairs report the owner for trait paths).
                        let ty: BasicTypeEnum<'ctx> = *gty;
                        if ty.is_struct_type() {
                            if let Some(tn) = self.pair_owner_of(ty.into_struct_type()) {
                                return Ok(crate::sema::Ty::Struct(tn));
                            }
                            if let Ok(sname) = self.ty_to_struct_name(&ty) {
                                if self.enum_types.contains_key(&sname) {
                                    return Ok(crate::sema::Ty::Enum(sname));
                                }
                                return Ok(crate::sema::Ty::Struct(sname));
                            }
                            return Err(CodegenError{message: format!("cannot infer type of {name}"), span: expr.span});
                        } else if ty.is_int_type() {
                            let bw = ty.into_int_type().get_bit_width();
                            if bw == 1 { return Ok(crate::sema::Ty::Bool); } else { return Ok(crate::sema::Ty::Int); }
                        } else if ty.is_pointer_type() {
                            // String globals (and other pointers).
                            return Ok(crate::sema::Ty::String);
                        } else if ty.is_array_type() {
                            return Ok(crate::sema::Ty::Array(Box::new(crate::sema::Ty::Int)));
                        }
                    }
                }
                if self.enum_types.contains_key(name) || self.enum_types.contains_key(lookup) {
                    let key = if self.enum_types.contains_key(name) { name } else { lookup };
                    return Ok(crate::sema::Ty::Enum(key.to_string()));
                }
                if self.struct_types.contains_key(name) || self.struct_types.contains_key(lookup) {
                    let key = if self.struct_types.contains_key(name) { name } else { lookup };
                    return Ok(crate::sema::Ty::Struct(key.to_string()));
                }
                Err(CodegenError{message: format!("cannot infer type of {name}"), span: expr.span})
            }
            ExprKind::This => {
                if let Some(cls) = &self.cur_class { return Ok(crate::sema::Ty::Struct(cls.clone())); }
                Err(CodegenError{message: "`this` outside method".into(), span: expr.span})
            }
            ExprKind::Super => {
                if let Some(cls) = &self.cur_class {
                    if let Some(parent) = self.class_extends.get(cls).cloned() {
                        return Ok(crate::sema::Ty::Struct(parent));
                    }
                    return Err(CodegenError{message: "`super` without parent".into(), span: expr.span});
                }
                Err(CodegenError{message: "`super` outside method".into(), span: expr.span})
            }
            ExprKind::MemberAccess { object, field, .. } => {
                let obj_ty = self.infer_expr_ty(object)?;
                if let crate::sema::Ty::Struct(ref sname) = obj_ty {
                    // Trait-typed receiver: field type from the first
                    // implementor (sema validated agreement across all).
                    if self.trait_names.contains(sname) {
                        return self.infer_trait_field(sname, field, expr.span);
                    }
                    let fields = self.struct_fields.get(sname).unwrap();
                    let idx = fields.get(field).unwrap();
                    let st = self.struct_types.get(sname).unwrap();
                    let fty = st.get_field_type_at_index(*idx).unwrap();
                    return self.llvm_field_to_sema(fty, expr.span);
                } else if let crate::sema::Ty::Enum(ref ename) = obj_ty {
                    if let Some(einfo) = self.enum_variant_tags.get(ename) {
                        if einfo.contains_key(field) {
                            return Ok(crate::sema::Ty::Enum(ename.clone()));
                        }
                    }
                    return Err(CodegenError{message: format!("enum `{}` has no variant `{}`", ename, field), span: expr.span});
                }
                Err(CodegenError{message: "member access inference on non-struct".into(), span: expr.span})
            }
            ExprKind::IntLit(_) => Ok(crate::sema::Ty::Int),
            ExprKind::FloatLit(_) => Ok(crate::sema::Ty::Double),
            ExprKind::BoolLit(_) => Ok(crate::sema::Ty::Bool),
            ExprKind::StringLit(_) => Ok(crate::sema::Ty::String),
            ExprKind::CharLit(_) => Ok(crate::sema::Ty::Char),
            ExprKind::Null => Ok(crate::sema::Ty::Any),
            ExprKind::Tuple(exprs) => {
                let mut tys = Vec::new();
                for e in exprs {
                    tys.push(self.infer_expr_ty(e)?);
                }
                Ok(crate::sema::Ty::Tuple(tys))
            }
            ExprKind::ArrayLit(elems) => {
                if elems.is_empty() {
                    return Ok(crate::sema::Ty::FixedArray {
                        elem: Box::new(crate::sema::Ty::Any),
                        size: Some(0),
                    });
                }
                let first = self.infer_expr_ty(&elems[0])?;
                Ok(crate::sema::Ty::FixedArray {
                    elem: Box::new(first),
                    size: Some(elems.len()),
                })
            }
            ExprKind::VecEmpty(_) => Ok(crate::sema::Ty::Vec(Box::new(crate::sema::Ty::Any))),
            ExprKind::MapLit { entries, .. } => {
                if entries.is_empty() {
                    return Ok(crate::sema::Ty::Map {
                        key: Box::new(crate::sema::Ty::Any),
                        value: Box::new(crate::sema::Ty::Any),
                    });
                }
                let k = self.infer_expr_ty(&entries[0].0)?;
                let v = self.infer_expr_ty(&entries[0].1)?;
                Ok(crate::sema::Ty::Map { key: Box::new(k), value: Box::new(v) })
            }
            ExprKind::Paren(inner) => self.infer_expr_ty(inner),
            ExprKind::Call { callee, .. } => {
                // Async-6: calls to known functions return the declared
                // type (async callees yield `task<Ret>`).
                if let Some((_, info)) = self.funcs.get(callee.as_str()) {
                    if info.is_async {
                        return Ok(crate::sema::Ty::Task(Box::new(info.ret.clone())));
                    }
                    return Ok(info.ret.clone());
                }
                Err(CodegenError{message: format!("cannot infer type of call `{callee}`"), span: expr.span})
            }
            ExprKind::Await { task, .. } => {
                match self.infer_expr_ty(task)? {
                    crate::sema::Ty::Task(inner) => Ok(*inner),
                    _ => Err(CodegenError{message: "`await` on non-task".into(), span: expr.span}),
                }
            }
            ExprKind::Spawn { task, .. } => {
                match self.infer_expr_ty(task)? {
                    t @ crate::sema::Ty::Task(_) => Ok(t),
                    _ => Err(CodegenError{message: "`spawn` on non-async call".into(), span: expr.span}),
                }
            }
            _ => Err(CodegenError{message: "cannot infer type of this expr for struct GEP".into(), span: expr.span}),
        }
    }

    fn lookup_var(
        &self,
        name: &str,
    ) -> Option<(PointerValue<'ctx>, BasicTypeEnum<'ctx>)> {
        for scope in self.vars.iter().rev() {
            if let Some(v) = scope.get(name) {
                return Some(*v);
            }
        }
        if let Some(v) = self.globals.get(name) {
            return Some(*v);
        }
        let lookup = name.rsplit("::").next().unwrap_or(name);
        if lookup != name {
            for scope in self.vars.iter().rev() {
                if let Some(v) = scope.get(lookup) {
                    return Some(*v);
                }
            }
            if let Some(v) = self.globals.get(lookup) {
                return Some(*v);
            }
        }
        None
    }
}

pub fn compile_to_object(
    program: &Program,
    obj_path: &Path,
    opt: OptLevel,
) -> Result<(), String> {
    let context = Context::create();
    let mut cg = Codegen::new(&context, "hella");
    cg.release = opt == OptLevel::Release;
    cg.compile_program(program).map_err(|e| {
        format!("{} at {}..{}", e.message, e.span.start, e.span.end)
    })?;
    // NOTE: no `module.verify()` on Windows: `LLVMVerifyModule`
    // segfaults (STATUS_ACCESS_VIOLATION) there for ordinary modules
    // whose function-level checks all pass (upstream Windows LLVM
    // builds use rpmalloc). Every construct is already verified at its
    // own lowering site, so this stays as defense in depth everywhere
    // else before object emission.
    #[cfg(not(windows))]
    cg.module.verify().map_err(|e| e.to_string())?;
    // OS-conditional object emission (see `target_machine` below):
    // POSIX keeps the fast in-process `write_to_file` path untouched.
    // Windows routes through `clang -c` on dumped IR instead — the
    // in-process COFF emitter (`LLVMTargetMachineEmitToFile`) crashes
    // nondeterministically there (STATUS_ACCESS_VIOLATION /
    // STATUS_STACK_BUFFER_OVERRUN, different survivors per run) for the
    // same modules that verify and emit IR fine, matching the two
    // earlier rpmalloc heap-crossing workarounds (`get_module_ir`,
    // `module.verify`). `opt` is honored via the `clang -O` flag.
    #[cfg(not(windows))]
    {
        let machine = target_machine(opt)?;
        if opt == OptLevel::Release {
            cg.optimize_for_release(&machine)?;
        }
        machine
            .write_to_file(&cg.module, inkwell::targets::FileType::Object, obj_path)
            .map_err(|e| e.to_string())?;
        Ok(())
    }
    #[cfg(windows)]
    {
        windows_compile_ir_to_object(&cg, obj_path, opt)
    }
}

/// Windows-only object emission: dump IR via the crash-safe
/// `print_to_file` round-trip (`get_module_ir`, never `print_to_string`)
/// and assemble it out-of-process with the same C driver used for the
/// final link (`$HELLA_LINKER`, else `clang`/`cc`). Keeps IR building
/// in-process (proven fine by the green unit tests and `--emit-llvm`);
/// only the crashing in-process COFF emitter is bypassed. Not used on
/// POSIX — see `compile_to_object`.
#[cfg(windows)]
fn windows_compile_ir_to_object(
    cg: &Codegen<'_>,
    obj_path: &Path,
    opt: OptLevel,
) -> Result<(), String> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static N: AtomicU64 = AtomicU64::new(0);
    let ll_path = std::env::temp_dir().join(format!(
        "hella-obj-{}-{}.ll",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed),
    ));
    cg.module
        .print_to_file(&ll_path)
        .map_err(|e| e.to_string())?;
    let linker = std::env::var("HELLA_LINKER")
        .ok()
        .filter(|l| !l.trim().is_empty())
        .or_else(|| {
            ["clang", "cc"]
                .into_iter()
                .find(|c| {
                    std::process::Command::new(c)
                        .arg("--version")
                        .output()
                        .is_ok()
                })
                .map(str::to_string)
        })
        .ok_or_else(|| {
            "no C compiler found (tried `clang`, `cc`) — install LLVM (https://releases.llvm.org/download.html) or set HELLA_LINKER".to_string()
        })?;
    let opt_flag = match opt {
        OptLevel::Debug => "-O0",
        OptLevel::Release => "-O3",
    };
    let status = std::process::Command::new(&linker)
        .arg("-c")
        .arg(opt_flag)
        .arg(&ll_path)
        .arg("-o")
        .arg(obj_path)
        .status()
        .map_err(|e| format!("failed to invoke {linker} for object emission: {e}"))?;
    let _ = std::fs::remove_file(&ll_path);
    if !status.success() {
        return Err(format!("object emission failed with {linker}"));
    }
    Ok(())
}

/// Target machine for object emission (also hands target info to the
/// release pass pipeline). Factored out so `--emit-llvm --release` can run
/// the same passes before dumping IR.
pub fn target_machine(
    opt: OptLevel,
) -> Result<inkwell::targets::TargetMachine, String> {
    // Host-only init: we always emit for the default (host) triple below,
    // and initializing every backend references LLVM target libs that some
    // distributions (notably the upstream Windows tarball) do not ship,
    // breaking the link with unresolved LLVMInitialize*Target symbols.
    inkwell::targets::Target::initialize_native(
        &inkwell::targets::InitializationConfig::default(),
    )
    .map_err(|e| e.to_string())?;
    let triple = inkwell::targets::TargetMachine::get_default_triple();
    let target =
        inkwell::targets::Target::from_triple(&triple).map_err(|e| e.to_string())?;
    target
        .create_target_machine(
            &triple,
            "generic",
            "",
            opt.machine_level(),
            inkwell::targets::RelocMode::Default,
            inkwell::targets::CodeModel::Default,
        )
        .ok_or_else(|| "failed to create target machine".to_string())
}

/// Optimization level for object emission (`hella build [--release]`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum OptLevel {
    /// No IR passes; machine `Default` — fast compiles, debuggable output.
    Debug,
    /// Standard O3 IR passes + `Aggressive` machine — slower compiles,
    /// faster binaries.
    Release,
}

impl OptLevel {
    fn machine_level(self) -> inkwell::OptimizationLevel {
        match self {
            OptLevel::Debug => inkwell::OptimizationLevel::Default,
            OptLevel::Release => inkwell::OptimizationLevel::Aggressive,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compile_src(src: &str) {
        let lexed = crate::lexer::lex(src);
        assert!(lexed.errors.is_empty(), "lex errors: {:?}", lexed.errors);
        let prog = crate::parse::parse(lexed.tokens, src.to_string()).unwrap();
        let ctx = inkwell::context::Context::create();
        let mut cg = Codegen::new(&ctx, "test");
        cg.compile_program(&prog)
            .expect("codegen failed");
    }

    fn compile_ir(src: &str) -> String {
        let lexed = crate::lexer::lex(src);
        assert!(lexed.errors.is_empty(), "lex errors: {:?}", lexed.errors);
        let prog = crate::parse::parse(lexed.tokens, src.to_string()).unwrap();
        let ctx = inkwell::context::Context::create();
        let mut cg = Codegen::new(&ctx, "test");
        cg.compile_program(&prog).expect("codegen failed");
        cg.get_module_ir()
    }

    /// Regression: `void` extension methods used to emit `ret i64 0`,
    /// failing module verification (`extension U::ex verify failed`).
    #[test]
    fn void_extension_method_verifies() {
        compile_src("class U has\nend\nextend U do\nvoid ex() do\nreturn\nend\nend\n");
    }

    #[test]
    fn void_extension_method_fallthrough_verifies() {
        compile_src("class U has\nend\nextend U do\nvoid ex() do\nint x = 1\nend\nend\n");
    }

    #[test]
    fn int_extension_method_verifies() {
        compile_src("class U has\nend\nextend U do\nint ex() do\nreturn 1\nend\nend\n");
    }

    /// Trait objects: `{data ptr, type tag}` pair lowering with switch
    /// dispatch over implementors (single + multi), field access, and
    /// trait-typed params/returns.
    #[test]
    fn trait_single_implementor_verifies() {
        compile_src(
            "trait Raf has\n  string nnn()\nend\nopen class User implements Raf has\n  public string name\n  User(string name) initialize\n  public string nnn() do\n    return this.name\n  end\nend\nvoid main() do\n  Raf u = User(\"a\")\n  string s = u.nnn()\nend\n",
        );
    }

    #[test]
    fn trait_multi_implementor_dispatch_verifies() {
        compile_src(
            "trait Speaker has\n  string speak()\nend\nopen class Cat implements Speaker has\n  Cat() initialize\n  public string speak() do\n    return \"meow\"\n  end\nend\nopen class Dog implements Speaker has\n  Dog() initialize\n  public string speak() do\n    return \"woof\"\n  end\nend\nstring pick(Speaker s) do\n  return s.speak()\nend\nvoid main() do\n  Speaker a = Cat()\n  Speaker b = Dog()\n  string x = pick(a)\n  string y = pick(b)\nend\n",
        );
    }

    /// `ref`/`out` params lower to pointers on both sides of every call
    /// (free functions, methods, ctors, operators, extensions). Used to
    /// fail verification with mismatched call signatures.
    #[test]
    fn ref_out_params_verify() {
        compile_src(
            "void swap(ref int a, ref int b) do\n  int t = a\n  a = b\n  b = t\nend\nvoid produce(out int v) do\n  v = 42\nend\nint main() do\n  int x = 1\n  int y = 2\n  swap(ref x, ref y)\n  produce(out x)\n  return 0\nend\n",
        );
        compile_src(
            "class Acc has\n  int total\n  Acc() initialize\n  public void bump(ref int x) do\n    x = x + 1\n  end\nend\nvoid main() do\n  Acc a = Acc()\n  int v = 10\n  a.bump(ref v)\nend\n",
        );
    }

    /// Implicit `out` declarations materialize their slot at the call:
    /// `out message` with no prior declaration brings the name into scope.
    #[test]
    fn implicit_out_var_verifies() {
        compile_src(
            "void some_fn(string name, out string msg) do\n  msg = \"Yo!\"\nend\nvoid main() do\n  string usr = \"Abbas\"\n  some_fn(usr, out message)\n  string s = message\nend\n",
        );
    }

    #[test]
    fn implicit_out_var_annotated_verifies() {
        compile_src(
            "void some_fn(string name, out string msg) do\n  msg = \"Yo!\"\nend\nvoid main() do\n  some_fn(\"x\", out string message)\nend\n",
        );
    }

    #[test]
    fn trait_field_access_and_return_verifies() {
        compile_src(
            "trait Named has\n  string label()\nend\nopen class User implements Named has\n  public string name\n  User(string name) initialize\n  public string label() do\n    return this.name\n  end\nend\nNamed make() do\n  return User(\"a\")\nend\nvoid main() do\n  Named u = make()\n  u.name = \"b\"\nend\n",
        );
    }

    /// Trait-typed locals register for destruction: the IR must call the
    /// implementor's destructor through the tag switch at scope exit.
    #[test]
    fn trait_local_destructor_called() {
        let ir = compile_ir(
            "trait Talker has\n  void f()\nend\nopen class Bot implements Talker has\n  Bot() initialize\n  public void f() do\n    return\n  end\n  ~Bot() do\n    return\n  end\nend\nvoid main() do\n  Talker t = Bot()\nend\n",
        );
        assert!(
            ir.contains("Bot__dtor") && ir.contains("trait.dtor"),
            "expected tag-dispatched dtor call, got:\n{ir}"
        );
    }

    /// A trait with no destructors anywhere emits no dtor machinery.
    #[test]
    fn trait_without_destructor_no_call() {
        let ir = compile_ir(
            "trait Talker has\n  void f()\nend\nopen class Bot implements Talker has\n  Bot() initialize\n  public void f() do\n    return\n  end\nend\nvoid main() do\n  Talker t = Bot()\nend\n",
        );
        assert!(
            !ir.contains("trait.dtor"),
            "unexpected dtor dispatch, got:\n{ir}"
        );
    }

    /// T-17: `|` alternatives, tuple patterns, and qualified enum patterns
    /// lower without panics (used to `panic!("enum pattern on non-enum")`).
    #[test]
    fn match_alternative_tuple_qualified_verifies() {
        compile_src(
            "enum Color has\n  Red\n  Green\n  Blue\nend\nint pick(Color c) do\n  return match c do\n    Color.Red | Color.Green -> 1\n    Color.Blue -> 2\n    _ -> 3\n  end\nend\nint tup((int, int) t) do\n  return match t do\n    (1, 2) -> 10\n    _ -> 20\n  end\nend\nvoid main() do\nend\n",
        );
    }

    /// T-17: single-letter enum names must not trip generic (`T` → `i64`)
    /// erasure in match lowering.
    #[test]
    fn single_letter_enum_match_verifies() {
        compile_src(
            "enum E has\n  A\n  B\nend\nint use(E e) do\n  return match e do\n    E.A -> 1\n    E.B -> 2\n  end\nend\nvoid main() do\nend\n",
        );
    }

    /// T-18: `for` over computed iters (literals, not just `Ident` vars).
    #[test]
    fn for_over_array_literal_verifies() {
        compile_src(
            "void main() do\n  for x in [10, 20, 30] do\n  end\nend\n",
        );
    }

    /// `for i in a..b` iterates lazily (never as a map: the range struct
    /// `{i64, i64, i1}` shares the 3-field shape, which used to hang).
    #[test]
    fn for_over_range_uses_range_bound() {
        let ir = compile_ir(
            "void main() do\n  for i in 0..3 do\n  end\nend\n",
        );
        assert!(
            ir.contains("for.range.span"),
            "expected lazy range bound, got:\n{ir}"
        );
    }

    /// T-19: `int main(string[] args)` lowers to C `i32 (i32, ptr)`.
    #[test]
    fn main_with_args_has_argc_argv() {
        let ir = compile_ir(
            "int main(string[] args) do\n  return 0\nend\n",
        );
        assert!(
            ir.contains("define i32 @main(i32") && ir.contains("argv.copy"),
            "expected argc/argv main, got:\n{ir}"
        );
    }

    /// `args.len()` loads the true argc from `__hella_argc` (not the
    /// static 16 slots); ordinary arrays keep their static size.
    #[test]
    fn args_len_observes_argc() {
        let ir = compile_ir(
            "int main(string[] args) do\n  return args.len()\nend\n",
        );
        assert!(
            ir.contains("__hella_argc"),
            "expected argc slot load for args.len(), got:\n{ir}"
        );
        let ir = compile_ir(
            "void main() do\n  int arr[4] nums\n  int n = nums.len()\nend\n",
        );
        assert!(
            !ir.contains("__hella_argc"),
            "ordinary arrays must not touch the argc slot, got:\n{ir}"
        );
    }

    /// `__hella_progname()` never emits a call: it loads the `__hella_argv0`
    /// global captured in main's prologue. Every main form declares
    /// `(i32 argc, ptr argv)` at LLVM level so this works with or without
    /// a Hella-level `args` parameter.
    #[test]
    fn progname_loads_argv0_without_call() {
        let src = "extern \"c\" from \"libc\" do\n    string __hella_progname()\nend\nvoid main() do\n    string p = __hella_progname()\nend\n";
        let ir = compile_ir(src);
        assert!(
            ir.contains("define i32 @main(i32"),
            "every main takes (argc, argv), got:\n{ir}"
        );
        assert!(
            ir.contains("__hella_argv0"),
            "expected argv0 capture, got:\n{ir}"
        );
        // The extern declaration may exist, but no call to it must be
        // emitted (there is no such libc symbol).
        let calls_it = ir
            .lines()
            .any(|l| l.trim_start().starts_with("call ") && l.contains("__hella_progname"));
        assert!(
            !calls_it,
            "the intrinsic call must not be emitted, got:\n{ir}"
        );
    }

    /// T-10: `debug_assert` emits in debug but vanishes in release.
    #[test]
    fn debug_assert_stripped_in_release() {
        let src = "void main() do\n  debug_assert true\nend\n";
        let debug_ir = compile_ir(src);
        assert!(debug_ir.contains("assert"), "debug build should keep debug_assert, got:\n{debug_ir}");
        let lexed = crate::lexer::lex(src);
        let prog = crate::parse::parse(lexed.tokens, src.to_string()).unwrap();
        let ctx = inkwell::context::Context::create();
        let mut cg = Codegen::new(&ctx, "test");
        cg.release = true;
        cg.compile_program(&prog).expect("codegen failed");
        let release_ir = cg.get_module_ir();
        assert!(
            !release_ir.contains("assert.fail") && !release_ir.contains("abort"),
            "release build should strip debug_assert, got:\n{release_ir}"
        );
    }

    /// T-6: array slicing lowers to a real copy (not object identity).
    #[test]
    fn slice_verifies() {
        compile_src(
            "void main() do\n  int arr[6] nums\n  int arr sub = nums[1..4]\n  int arr full = nums[..]\n  int arr incl = nums[1..=2]\nend\n",
        );
    }

    /// T-6: vector and string slices lower too (fresh vec value / malloc'd copy).
    #[test]
    fn slice_vec_string_verifies() {
        compile_src(
            "void main() do\n  int vec v = vec[]\n  int vec w = v[1..3]\n  string s = \"hello\"\n  string t = s[1..4]\nend\n",
        );
    }

    /// T-9: tuple-returning functions lower struct-by-value (not `ptr`),
    /// so destructuring binds correctly.
    #[test]
    fn tuple_return_destructure_verifies() {
        compile_src(
            "(int, int) pair() do\n  return (3, 4)\nend\nvoid main() do\n  a, b = pair()\n  int x = 1\n  int y = 2\n  x, y = (y, x)\nend\n",
        );
    }

    /// T-13: multi-param payloads lower through the wide `{tag, words}`
    /// layout, including heterogeneous positions.
    #[test]
    fn multi_payload_enum_verifies() {
        compile_src(
            "enum P has\n  Pair(int a, int b)\n  Single(int x)\n  Mix(int n, string s)\nend\nint sum(P p) do\n  return match p do\n    .Pair(a, b) -> a + b\n    .Single(x) -> x\n    .Mix(n, s) -> n\n  end\nend\nvoid main() do\n  P p = .Pair(3, 4)\nend\n",
        );
    }

    /// Own-P2a: structs with `own` fields lower scope-exit destruction
    /// (an observable `free` on the owned pair).
    #[test]
    fn struct_own_field_destroyed() {
        let ir = compile_ir(
            "struct Pet has\n  string name\nend\nstruct Owner has\n  own Pet pet\n  int level\nend\nvoid main() do\n  Owner o = Owner has\n    pet = new Pet(\"r\")\n    level = 1\n  end\nend\n",
        );
        assert!(
            ir.contains("call void @free"),
            "expected heap free for owned field, got:\n{ir}"
        );
    }

    /// Own-P2a: member chains read through `own` fields.
    #[test]
    fn nested_own_field_access_verifies() {
        compile_src(
            "struct Pet has\n  string name\nend\nstruct Owner has\n  own Pet pet\nend\nstring fetch(Owner o) do\n  return o.pet.name\nend\nvoid main() do\nend\n",
        );
    }

    /// T-4: Optional lift (`int` → `int?`) and `??` lower through the
    /// `{value, present}` representation.
    #[test]
    fn optional_coalesce_verifies() {
        compile_src(
            "int orElse(int? n, int fallback) do\n  return n ?? fallback\nend\nvoid main() do\n  int? a = 5\n  int x = orElse(a, 99)\nend\n",
        );
    }

    /// T-7: `super.method()` dispatches to the parent implementation and
    /// expression-`Self` behaves as `this`.
    #[test]
    fn super_and_self_verifies() {
        compile_src(
            "open class Base has\n  public int v\n  Base(int v) initialize\n  public int getv() do\n    return this.v\n  end\nend\nopen class Child extends Base has\n  Child(int v) initialize\n  public override int getv() do\n    return super.getv() + 1\n  end\n  public int viagetv() do\n    return Self.getv()\n  end\nend\nvoid main() do\n  Child c = Child(10)\nend\n",
        );
    }

    /// T-3: named functions decay to pointers for `function<Ret(Args)>` slots.
    #[test]
    fn function_reference_verifies() {
        compile_src(
            "int apply(function<int(int)> f, int x) do\n  return f(x)\nend\nint twice(int x) do\n  return x * 2\nend\nvoid main() do\n  int r = apply(twice, 21)\nend\n",
        );
    }

    /// Own-P1-1: `is`/`is not` on `own` lowers to data-pointer identity.
    #[test]
    fn own_is_identity_verifies() {
        compile_src(
            "open class User has\n  User() initialize\nend\nvoid main() do\n  own User a = new User()\n  own User b = new User()\n  bool same = a is b\n  bool diff = a is not b\nend\n",
        );
    }

    /// Own-P1-4: methods with `own` params track + destroy them.
    #[test]
    fn method_own_param_verifies() {
        compile_src(
            "open class User has\n  User() initialize\n  public void greet(own User friend, bool early) do\n    if early do\n      return\n    end\n  end\nend\nvoid main() do\n  own User a = new User()\n  a.greet(new User(), true)\nend\n",
        );
    }

    /// Own-P1-3: conditional moves into `own` slots verify.
    #[test]
    fn ternary_own_move_verifies() {
        compile_src(
            "open class User has\n  User() initialize\nend\nvoid main() do\n  own User a = new User()\n  own User b = new User()\n  bool pick = true\n  own User c = pick ? a : b\nend\n",
        );
    }

    /// T-11: omitted trailing defaulted args lower via fill.
    #[test]
    fn default_args_verifies() {
        compile_src(
            "int add(int a, int b = 10) do\n  return a + b\nend\nint mul(int a = 3, int b = 4) do\n  return a * b\nend\nvoid main() do\n  int x = add(1)\n  int y = mul()\n  int z = add(1, 2)\nend\n",
        );
        compile_src(
            "class Counter has\n  int n\n  Counter(int n) initialize\n  int bump(int step = 1) do\n    return this.n + step\n  end\nend\nvoid main() do\n  Counter c = Counter(10)\n  int x = c.bump()\nend\n",
        );
    }

    /// T-21: conversions are typed by their declared target.
    #[test]
    fn conversion_to_string_verifies() {
        compile_src(
            "class Meters has\n  int v\n  Meters(int v) initialize\n  convert Meters to string do\n    return \"m\"\n  end\nend\nvoid main() do\n  Meters m = Meters(5)\nend\n",
        );
    }
}
