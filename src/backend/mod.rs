use std::collections::HashMap;
use std::fmt::Write;

use cranelift::prelude::{*, isa::CallConv, codegen::Context};
use cranelift_module::{DataContext, Module, Linkage};
use cranelift_object::{ObjectModule, ObjectBuilder};
use target_lexicon::triple;

use crate::frontend::ast_lowering::{SExpr, Type as SExprType, LValue};

pub struct Generator {
    builder_context: FunctionBuilderContext,
    ctx: Context,
    data_ctx: DataContext,
    module: ObjectModule,
}

impl Default for Generator {
    fn default() -> Self {
        use std::str::FromStr;

        let mut b = settings::builder();
        b.set("opt_level", "speed_and_size").unwrap();

        let f = settings::Flags::new(b);
        let isa_data = isa::lookup(triple!("x86_64-elf")).unwrap().finish(f);
        let builder = ObjectBuilder::new(isa_data, "x86_64", cranelift_module::default_libcall_names()).unwrap();
        let module = ObjectModule::new(builder);
        Self {
            builder_context: FunctionBuilderContext::new(),
            ctx: module.make_context(),
            data_ctx: DataContext::new(),
            module,
        }
    }
}

impl Generator {
    pub fn compile(&mut self, sexprs: Vec<SExpr<'_>>) {
        self.translate(&sexprs);
    }

    fn translate(&mut self, sexprs: &[SExpr<'_>]) {
        for sexpr in sexprs {
            if let SExpr::FuncDef { name, ret_type, args, expr, .. } = sexpr {
                if args.iter().any(|(_, v)| v.has_generic()) || ret_type.has_generic() {
                    continue;
                }

                for (_, typ) in args {
                    let typ = Self::convert_type_to_type(typ);
                    self.ctx.func.signature.params.push(AbiParam::new(typ));
                }

                let typ = Self::convert_type_to_type(ret_type);
                self.ctx.func.signature.returns.push(AbiParam::new(typ));

                let mut func = self.ctx.func.clone();
                let mut builder = FunctionBuilder::new(&mut func, &mut self.builder_context);
                let entry_block = builder.create_block();
                builder.append_block_params_for_function_params(entry_block);
                builder.switch_to_block(entry_block);
                let mut var_map = vec![HashMap::new()];
                let mut var_index = 0;
                for (i, (name, typ)) in args.iter().enumerate() {
                    let var = Variable::new(var_index);
                    var_map[0].insert(*name, var);
                    var_index += 1;
                    builder.declare_var(var, Self::convert_type_to_type(typ));
                    builder.def_var(var, builder.block_params(entry_block)[i]);
                }

                // TODO: returning strings
                let ret_value = Self::translate_expr(&**expr, &mut builder, &mut var_map, &mut var_index, None, &mut self.module, &mut self.ctx, &mut self.data_ctx);
                builder.ins().return_(&[ret_value]);
                builder.seal_all_blocks();
                println!("{}", builder.func);
                builder.finalize();
                self.ctx.func = func;

                let id = self.module.declare_function(&Self::mangle_func(name, args.iter().map(|(_, v)| v), ret_type), Linkage::Export, &self.ctx.func.signature).unwrap();
                self.module.define_function(id, &mut self.ctx).unwrap();
                self.module.clear_context(&mut self.ctx);
                var_map.clear();
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn translate_expr<'a>(sexpr: &SExpr<'a>, builder: &mut FunctionBuilder, var_map: &mut Vec<HashMap<&'a str, Variable>>, var_index: &mut usize, break_block: Option<Block>, module: &mut ObjectModule, ctx: &mut Context, data_ctx: &mut DataContext) -> Value {
        #[allow(unused)]
        match sexpr {
            SExpr::Int { meta, value } => {
                if matches!(meta.type_, SExprType::Int(_, 1)) {
                    builder.ins().bconst(Self::convert_type_to_type(&meta.type_), *value != 0)
                } else {
                    builder.ins().iconst(Self::convert_type_to_type(&meta.type_), *value as i64)
                }
            }

            SExpr::Symbol { meta, value } => {
                for scope in var_map.iter().rev() {
                    if let Some(&var) = scope.get(value) {
                        return builder.use_var(var);
                    }
                }

                let mut sig = Signature::new(CallConv::SystemV);
                if let SExprType::Function(a, r) = &meta.type_ {
                    sig.params.extend(a.iter().map(Self::convert_type_to_type).map(AbiParam::new));
                    sig.returns.push(AbiParam::new(Self::convert_type_to_type(&**r)));
                    let func = module.declare_function(&Self::mangle_func(value, a.iter(), &**r), Linkage::Import, &sig).unwrap();
                    let func = module.declare_func_in_func(func, builder.func);

                    builder.ins().func_addr(Self::convert_type_to_type(&meta.type_), func)
                } else {
                    unreachable!("func must have type func");
                }
            }

            SExpr::Float { meta, value } => {
                if meta.type_ == SExprType::F32 {
                    builder.ins().f32const(*value as f32)
                } else {
                    builder.ins().f64const(*value)
                }
            }

            SExpr::Str { meta, value } => {
                let name = format!("{}", var_index);
                *var_index += 1;
                data_ctx.define(Box::from(value.as_bytes()));
                let sym = module.declare_data(&name, Linkage::Hidden, false, false).unwrap();
                module.define_data(sym, data_ctx).unwrap();
                data_ctx.clear();
                let val = module.declare_data_in_func(sym, builder.func);
                let size = builder.ins().iconst(Self::convert_type_to_type(&SExprType::Int(false, 64)), value.len() as i64);
                let reference = builder.ins().global_value(Self::convert_type_to_type(&meta.type_), val);
                let slot = builder.create_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 16));
                builder.ins().stack_store(size, slot, 0);
                builder.ins().stack_store(reference, slot, 8);
                builder.ins().stack_addr(Self::convert_type_to_type(&meta.type_), slot, 0)
            }

            SExpr::Seq { values, .. } => {
                var_map.push(HashMap::new());
                for value in values[..values.len() - 1].iter() {
                    Self::translate_expr(value, builder, var_map, var_index, break_block, module, ctx, data_ctx);
                }

                let v = Self::translate_expr(values.last().unwrap(), builder, var_map, var_index, break_block, module, ctx, data_ctx);
                var_map.pop();
                v
            }

            SExpr::Cond { meta, values, elsy } => {
                let mut conds = vec![builder.current_block().unwrap()];
                let mut thens = vec![];
                for _ in values {
                    conds.push(builder.create_block());
                    thens.push(builder.create_block());
                }

                let last = builder.create_block();

                builder.append_block_param(last, Self::convert_type_to_type(&meta.type_));

                for (i, (cond, body)) in values.iter().enumerate() {
                    var_map.push(HashMap::new());
                    let cond = Self::translate_expr(cond, builder, var_map, var_index, break_block, module, ctx, data_ctx);
                    builder.ins().brz(cond, conds[i + 1], &[]);
                    builder.ins().jump(thens[i], &[]);
                    builder.switch_to_block(thens[i]);
                    let then = Self::translate_expr(body, builder, var_map, var_index, break_block, module, ctx, data_ctx);
                    var_map.pop();
                    builder.ins().jump(last, &[then]);
                    builder.switch_to_block(conds[i + 1]);
                }

                let elsy = if let Some(elsy) = elsy {
                    var_map.push(HashMap::new());
                    let v = Self::translate_expr(&**elsy, builder, var_map, var_index, break_block, module, ctx, data_ctx);
                    var_map.pop();
                    v
                } else {
                    builder.ins().bconst(Self::convert_type_to_type(&SExprType::Tuple(vec![])), false)
                };
                builder.ins().jump(last, &[elsy]);

                builder.switch_to_block(last);
                builder.block_params(last)[0]
            }

            SExpr::Loop { meta, value } => {
                let loop_block = builder.create_block();
                let break_block = builder.create_block();
                builder.append_block_param(break_block, Self::convert_type_to_type(&meta.type_));
                builder.ins().jump(loop_block, &[]);
                builder.switch_to_block(loop_block);
                var_map.push(HashMap::new());
                Self::translate_expr(&**value, builder, var_map, var_index, Some(break_block), module, ctx, data_ctx);
                var_map.pop();
                builder.ins().jump(loop_block, &[]);
                builder.switch_to_block(break_block);
                builder.block_params(break_block)[0]
            }

            SExpr::Break { meta, value } => {
                let value = if let Some(value) = value {
                    Self::translate_expr(&**value, builder, var_map, var_index, break_block, module, ctx, data_ctx)
                } else {
                    builder.ins().bconst(Self::convert_type_to_type(&meta.type_), false)
                };

                builder.ins().jump(break_block.unwrap(), &[value]);
                let new = builder.create_block();
                builder.switch_to_block(new);
                builder.ins().bconst(Self::convert_type_to_type(&meta.type_), false)
            }

            SExpr::Nil { meta } => builder.ins().bconst(Self::convert_type_to_type(&meta.type_), false),

            SExpr::Type { value, .. } => Self::translate_expr(value, builder, var_map, var_index, break_block, module, ctx, data_ctx),

            SExpr::FuncCall { meta, func, values } => {
                let args = values;
                let mut values: Vec<_> = args.iter().map(|v| Self::translate_expr(v, builder, var_map, var_index, break_block, module, ctx, data_ctx)).collect();
                match &**func {
                    SExpr::Symbol { value: "+", .. } => {
                        if meta.type_ == SExprType::F32 || meta.type_ == SExprType::F64 {
                            builder.ins().fadd(values[0], values[1])
                        } else {
                            builder.ins().iadd(values[0], values[1])
                        }
                    }

                    SExpr::Symbol { value: "-", .. } => {
                        if meta.type_ == SExprType::F32 || meta.type_ == SExprType::F64 {
                            builder.ins().fsub(values[0], values[1])
                        } else {
                            builder.ins().isub(values[0], values[1])
                        }
                    }

                    SExpr::Symbol { value: "*", .. } => {
                        if meta.type_ == SExprType::F32 || meta.type_ == SExprType::F64 {
                            builder.ins().fmul(values[0], values[1])
                        } else {
                            builder.ins().imul(values[0], values[1])
                        }
                    }

                    SExpr::Symbol { value: "/", .. } => {
                        if meta.type_ == SExprType::F32 || meta.type_ == SExprType::F64 {
                            builder.ins().fdiv(values[0], values[1])
                        } else if matches!(meta.type_, SExprType::Int(true, _)) {
                            builder.ins().sdiv(values[0], values[1])
                        } else {
                            builder.ins().udiv(values[0], values[1])
                        }
                    }

                    SExpr::Symbol { value: "%", .. } => {
                        if matches!(meta.type_, SExprType::Int(true, _)) {
                            builder.ins().srem(values[0], values[1])
                        } else {
                            builder.ins().urem(values[0], values[1])
                        }
                    }

                    SExpr::Symbol { value: "syscall", .. } => {
                        let mut syscall_sig = Signature::new(CallConv::SystemV);
                        syscall_sig.params.extend([AbiParam::new(Self::convert_type_to_type(&SExprType::Int(false, 64))); 7]);
                        syscall_sig.returns.push(AbiParam::new(Self::convert_type_to_type(&SExprType::Int(false, 64))));
                        let syscall = module.declare_function("syscall_", Linkage::Import, &syscall_sig).unwrap();
                        let syscall = module.declare_func_in_func(syscall, builder.func);

                        let call = builder.ins().call(syscall, &values);
                        builder.inst_results(call)[0]
                    }

                    SExpr::Symbol { value: "cast", .. } => {
                        match (&meta.type_, &args[0].meta().type_) {
                            (SExprType::Int(_, width1), SExprType::Int(_, width2)) => todo!(),
                            (SExprType::Pointer(_, _), SExprType::Int(_, _)) => values.remove(0),
                            (SExprType::Int(_, _), SExprType::Pointer(_, _)) => values.remove(0),
                            _ => todo!("casting into {:?} from {:?}", meta.type_, args[0].meta().type_),
                        }
                    }

                    SExpr::Symbol { value: "alloca", .. } => {
                        match &meta.type_ {
                            SExprType::Pointer(_, v) => {
                                let slot = builder.create_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, Self::size_of(&**v)));
                                builder.ins().stack_addr(Self::convert_type_to_type(&meta.type_), slot, 0)
                            }

                            SExprType::Slice(_, v) => {
                                const SIZE: u32 = 512;
                                let len = values.remove(0);
                                let size = builder.ins().imul_imm(len, Self::size_of(&**v) as i64);
                                let flags = builder.ins().icmp_imm(IntCC::UnsignedLessThanOrEqual, size, SIZE as i64);
                                builder.ins().trapz(flags, TrapCode::StackOverflow);
                                let slot = builder.create_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, SIZE));
                                let reference = builder.ins().stack_addr(Self::convert_type_to_type(&meta.type_), slot, 0);
                                let slot = builder.create_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 16));
                                builder.ins().stack_store(len, slot, 0);
                                builder.ins().stack_store(reference, slot, 8);
                                builder.ins().stack_addr(Self::convert_type_to_type(&meta.type_), slot, 0)
                            }

                            _ => unreachable!(),
                        }
                    }

                    SExpr::Symbol { value: "get", .. } => {
                        let i = values.remove(1);
                        let v = values.remove(0);
                        let v = builder.ins().load(Self::convert_type_to_type(&SExprType::Pointer(true, Box::new(SExprType::F32))), MemFlags::new(), v, 8);
                        let i = builder.ins().imul_imm(i, Self::size_of(&meta.type_) as i64);
                        let ptr = builder.ins().iadd(v, i);
                        builder.ins().load(Self::convert_type_to_type(&meta.type_), MemFlags::new(), ptr, 0)
                    }

                    SExpr::Symbol { value: "&", .. } => builder.ins().band(values.remove(0), values.remove(0)),
                    SExpr::Symbol { value: "|", .. } => builder.ins().bor(values.remove(0), values.remove(0)),
                    SExpr::Symbol { value: "^", .. } => builder.ins().bxor(values.remove(0), values.remove(0)),
                    SExpr::Symbol { value: "<<", .. } => builder.ins().ishl(values.remove(0), values.remove(0)),
                    SExpr::Symbol { value: ">>", .. } => builder.ins().ushr(values.remove(0), values.remove(0)),

                    SExpr::Symbol { value: "<", .. } => {
                        if meta.type_ == SExprType::F32 || meta.type_ == SExprType::F64 {
                            builder.ins().fcmp(FloatCC::LessThan, values[0], values[1])
                        } else if matches!(meta.type_, SExprType::Int(true, _)) {
                            builder.ins().icmp(IntCC::SignedLessThan, values[0], values[1])
                        } else {
                            builder.ins().icmp(IntCC::UnsignedLessThan, values[0], values[1])
                        }
                    }

                    SExpr::Symbol { meta, value } => {
                        let mut sig = Signature::new(CallConv::SystemV);
                        if let SExprType::Function(a, r) = &meta.type_ {
                            sig.params.extend(a.iter().map(Self::convert_type_to_type).map(AbiParam::new));
                            sig.returns.push(AbiParam::new(Self::convert_type_to_type(&**r)));
                        }

                        for scope in var_map.iter().rev() {
                            if let Some(func) = scope.get(value) {
                                let sig = builder.import_signature(sig);
                                let func = builder.use_var(*func);
                                let call = builder.ins().call_indirect(sig, func, &values);
                                return builder.inst_results(call)[0];
                            }
                        }

                        if let SExprType::Function(a, r) = &meta.type_ {
                            let func = module.declare_function(&Self::mangle_func(value, a.iter(), &**r), Linkage::Import, &sig).unwrap();
                            let func = module.declare_func_in_func(func, builder.func);

                            let call = builder.ins().call(func, &values);
                            builder.inst_results(call)[0]
                        } else {
                            unreachable!("must be func");
                        }
                    }

                    _ => todo!("{:?} can't be used as a function yet", func),
                }
            }

            SExpr::StructSet { meta, name, values } => todo!(),

            SExpr::Declare { meta, mutable, variable, value } => {
                let var = Variable::new(*var_index);
                *var_index += 1;
                builder.declare_var(var, Self::convert_type_to_type(&meta.type_));
                let val = Self::translate_expr(&**value, builder, var_map, var_index, break_block, module, ctx, data_ctx);
                var_map.last_mut().unwrap().insert(*variable, var);
                builder.def_var(var, val);
                builder.use_var(var)
            }

            SExpr::Assign { meta, lvalue: LValue::Symbol(variable), value } => {
                let mut var = Variable::new(0);
                for scope in var_map.iter().rev() {
                    if let Some(v) = scope.get(variable) {
                        var = *v;
                        break;
                    }
                }
                let val = Self::translate_expr(&**value, builder, var_map, var_index, break_block, module, ctx, data_ctx);
                builder.def_var(var, val);
                builder.use_var(var)
            }

            SExpr::Assign { meta, lvalue, value } => {
                let value = Self::translate_expr(&**value, builder, var_map, var_index, break_block, module, ctx, data_ctx);
                let lvalue = Self::get_pointer(lvalue, builder, var_map, var_index, break_block, module, ctx, data_ctx);
                builder.ins().store(MemFlags::new(), value, lvalue, 0);
                value
            }

            SExpr::Attribute { meta, top, attrs } => {
                let val = Self::translate_expr(top, builder, var_map, var_index, break_block, module, ctx, data_ctx);
                match &top.meta().type_ {
                    /*
                    Slices are of the following form:
                    struct Slice<T> {
                        len: u64,
                        ptr: *const T,
                    }
                    ie:
                    {
                        len: 00..063
                        ptr: 64..127
                    }
                    */
                    SExprType::Slice(_, typ) => {
                        match attrs[0] {
                            "len" | "cap" => {
                                builder.ins().load(Self::convert_type_to_type(&meta.type_), MemFlags::new(), val, 0)
                            }

                            "ptr" => {
                                builder.ins().load(Self::convert_type_to_type(&meta.type_), MemFlags::new(), val, 8)
                            }

                            _ => unreachable!(),
                        }
                    }

                    SExprType::Struct(_, _) => todo!(),
                    _ => unreachable!(),
                }
            }

            _ => todo!(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn get_pointer<'a>(lvalue: &LValue<'a>, builder: &mut FunctionBuilder, var_map: &mut Vec<HashMap<&'a str, Variable>>, var_index: &mut usize, break_block: Option<Block>, module: &mut ObjectModule, ctx: &mut Context, data_ctx: &mut DataContext) -> Value {
        match lvalue {
            LValue::Symbol(v) => {
                for scope in var_map.iter().rev() {
                    if let Some(var) = scope.get(v) {
                        return builder.use_var(*var);
                    }
                }

                unreachable!("variable was typechecked");
            }

            LValue::Attribute(_v, _attrs) => todo!(),

            LValue::Deref(v) => {
                let ret = Self::get_pointer(v, builder, var_map, var_index, break_block, module, ctx, data_ctx);
                if let LValue::Symbol(_) = **v {
                    ret
                } else {
                    builder.ins().load(Self::convert_type_to_type(&SExprType::Pointer(true, Box::new(SExprType::F32))), MemFlags::new(), ret, 0)
                }
            }

            LValue::Get(t, v, i) => {
                let i = Self::translate_expr(&**i, builder, var_map, var_index, break_block, module, ctx, data_ctx);
                let offset = builder.ins().imul_imm(i, Self::size_of(t) as i64);
                let ptr = Self::get_pointer(v, builder, var_map, var_index, break_block, module, ctx, data_ctx);
                let ptr = if let LValue::Symbol(_) = **v {
                    ptr
                } else {
                    builder.ins().load(Self::convert_type_to_type(&SExprType::Pointer(true, Box::new(SExprType::F32))), MemFlags::new(), ptr, 0)
                };
                let ptr = builder.ins().load(Self::convert_type_to_type(&SExprType::Pointer(true, Box::new(SExprType::F32))), MemFlags::new(), ptr, 8);
                builder.ins().iadd(ptr, offset)
            }
        }
    }

    fn convert_type_to_type(t: &SExprType) -> Type {
        match t {
            SExprType::Int(_, width) if *width == 1 => types::B1,
            SExprType::Int(_, width) if *width == 8 => types::I8,
            SExprType::Int(_, width) if *width == 16 => types::I16,
            SExprType::Int(_, width) if *width == 32 => types::I32,
            SExprType::Int(_, width) if *width == 64 => types::I64,
            SExprType::F32 => types::F32,
            SExprType::F64 => types::F64,

            SExprType::Tuple(v) if v.is_empty() => types::B1,

            SExprType::Pointer(_, _) => types::I64,
            SExprType::Slice(_, _) => types::I64,

            SExprType::Struct(_, _) => todo!(),

            SExprType::Function(_, _) => types::I64,

            _ => unreachable!(),
        }
    }

    fn size_of(t: &SExprType) -> u32 {
        match t {
            SExprType::Int(_, width) if *width == 1 => 1,
            SExprType::Int(_, width) if *width == 8 => 1,
            SExprType::Int(_, width) if *width == 16 => 2,
            SExprType::Int(_, width) if *width == 32 => 4,
            SExprType::Int(_, width) if *width == 64 => 8,
            SExprType::F32 => 4,
            SExprType::F64 => 8,

            SExprType::Tuple(v) if v.is_empty() => 0,

            SExprType::Pointer(_, _) => 8,
            SExprType::Slice(_, _) => 8,

            SExprType::Struct(_, _) => todo!(),

            SExprType::Function(_, _) => todo!(),

            _ => unreachable!(),
        }
    }

    fn mangle_type(mangled: &mut String, t: &SExprType) {
        match t {
            SExprType::Int(false, width) => write!(mangled, "u{}", width).unwrap(),
            SExprType::Int(true, width) => write!(mangled, "i{}", width).unwrap(),
            SExprType::F32 => mangled.push('f'),
            SExprType::F64 => mangled.push('F'),
            SExprType::Tuple(v) if v.is_empty() => mangled.push('N'),
            SExprType::Pointer(false, v) => {
                mangled.push('p');
                Self::mangle_type(mangled, &**v);
            }
            SExprType::Pointer(true, v) => {
                mangled.push('P');
                Self::mangle_type(mangled, &**v);
            }
            SExprType::Slice(false, v) => {
                mangled.push('s');
                Self::mangle_type(mangled, &**v);
            }
            SExprType::Slice(true, v) => {
                mangled.push('S');
                Self::mangle_type(mangled, &**v);
            }
            SExprType::Struct(_, _) => todo!(),
            SExprType::Function(a, r) => {
                mangled.push('U');
                for (i, a) in a.iter().enumerate() {
                    if i != 0 {
                        mangled.push(',');
                    }
                    Self::mangle_type(mangled, a);
                }
                mangled.push(':');
                Self::mangle_type(mangled, &**r);
            }

            _ => unreachable!(),
        }
    }

    fn mangle_func<'a, 'b>(name: &str, args: impl Iterator<Item=&'a SExprType<'b>>, ret: &SExprType) -> String
        where 'b: 'a
    {
        if name == "main" {
            return String::from("main");
        }

        let mut mangled = String::new();
        write!(mangled, "amy_{}@", name).unwrap();
        for (i, arg) in args.enumerate() {
            if i != 0 {
                mangled.push(',');
            }
            Self::mangle_type(&mut mangled, arg);
        }
        mangled.push(':');
        Self::mangle_type(&mut mangled, ret);
        mangled
    }

    pub fn emit_object(self) -> Vec<u8> {
        self.module.finish().emit().unwrap()
    }
}
