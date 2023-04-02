//! Implementation of the bytecode virtual machine.

use std::{
    cell::RefCell,
    ops::{Add, Deref, DerefMut, Div, Mul, Neg, Not, Sub},
    rc::Rc,
};

use rustc_hash::FxHashMap;

use crate::{
    compile::Parser,
    heap::Heap,
    object::{
        ObjBoundMethod, ObjClass, ObjClosure, ObjFun, ObjInstance, ObjNativeFun, ObjUpvalue,
        Object, ObjectError, RefBoundMethod, RefClass, RefClosure, RefNativeFun, RefUpvalue,
    },
    opcode::Opcode,
    value::{Value, ValueError},
    InterpretError,
};

#[cfg(feature = "dbg-execution")]
use crate::chunk::disassemble_instruction;

/// The max number of values can be put onto the virtual machine's stack.
const VM_STACK_SIZE: usize = 256;

/// The max number of call frames can be handled by the virtual machine.
const VM_FRAMES_MAX: usize = 64;

/// An enumeration of potential errors occur when running the bytecodes.
#[derive(Debug, Eq, PartialEq, thiserror::Error)]
pub enum RuntimeError {
    /// Can't parse a byte as an opcode.
    #[error(transparent)]
    InvalidOpcode(#[from] num_enum::TryFromPrimitiveError<Opcode>),

    /// Can't perform some operations given the current value(s).
    #[error(transparent)]
    Value(#[from] ValueError),

    /// Can't perform some operations given the current object(s).
    #[error(transparent)]
    Object(#[from] ObjectError),

    /// Overflow the virtual machine's stack.
    #[error("Stack overflow.")]
    StackOverflow,

    /// Can't access a property.
    #[error("Only instances have properties.")]
    ObjectHasNoProperty,

    /// Can't access a field.
    #[error("Only instances have fields.")]
    ObjectHasNoField,

    /// Can't find a variable in scope.
    #[error("Undefined variable '{0}'.")]
    UndefinedVariable(String),

    /// Can't find a property in the instance.
    #[error("Undefined property '{0}'.")]
    UndefinedProperty(String),

    /// Can't inherit objects that are not supported.
    #[error("Superclass must be a class.")]
    InvalidSuperclass,

    /// Can't call objects that are not supported.
    #[error("Can only call functions and classes.")]
    InvalidCallee,

    /// Can't invoke objects that are not supported.
    #[error("Only instances have methods.")]
    InvalidMethodInvocation,

    /// Called a function/method with incorrect number of arguments.
    #[error("Expected {arity} arguments but got {argc}.")]
    InvalidArgumentsCount {
        /// The arity of the function.
        arity: u8,
        /// The number of arguments given.
        argc: u8,
    },
}

/// A bytecode virtual machine for the Lox programming language.
pub struct VirtualMachine {
    stack: Vec<Value>,
    frames: Vec<CallFrame>,
    open_upvalues: Vec<RefUpvalue>,
    globals: FxHashMap<Rc<str>, Value>,
    grey_objects: Vec<Object>,
    heap: Heap,
}

impl Default for VirtualMachine {
    fn default() -> Self {
        Self::new()
    }
}

impl VirtualMachine {
    /// Create a new virtual machine that prints to the given output.
    pub fn new() -> Self {
        let mut vm = Self {
            stack: Vec::with_capacity(VM_STACK_SIZE),
            frames: Vec::with_capacity(VM_FRAMES_MAX),
            open_upvalues: Vec::new(),
            globals: FxHashMap::default(),
            grey_objects: Vec::new(),
            heap: Heap::default(),
        };
        vm.define_native("clock", 0, clock_native)
            .expect("Native function must be defined.");
        vm
    }
}

impl VirtualMachine {
    /// Compile and execute the given source code.
    pub fn interpret(&mut self, src: &str) -> Result<(), InterpretError> {
        let parser = Parser::new(src, &mut self.heap);
        let fun = parser.compile().ok_or(InterpretError::Compile)?;
        self.run(fun).map_err(|err| {
            eprintln!("{err}");
            if let Err(err) = self.trace_calls() {
                eprintln!("{err}");
            };
            self.stack.clear();
            InterpretError::Runtime
        })
    }

    fn run(&mut self, fun: ObjFun) -> Result<(), RuntimeError> {
        // Push the constant onto the stack so GC won't remove it while allocating the function.
        for constant in &fun.chunk.constants {
            self.stack_push(*constant)?;
        }
        let constant_count = fun.chunk.constants.len();
        let fun_object = self.alloc_fun(fun);
        // Remove all added constants.
        self.stack_remove_top(constant_count);

        // Push the function onto the stack so GC won't remove it while we allocating the closure.
        self.stack_push(Value::Object(fun_object))?;
        // Create a closure for the script function. Note that script can't have upvalues.
        let closure_object = self.alloc_closure(ObjClosure {
            fun: *fun_object.as_fun()?,
            upvalues: Vec::new(),
        });
        // Pop the fun object as we no longer need it.
        self.stack_pop();

        // Push the closure onto the stack so GC won't remove for the entire runtime.
        self.stack_push(Value::Object(closure_object))?;
        // Start running the closure.
        let mut task = Task::new(self);
        task.call_closure(*closure_object.as_closure()?, 0)
            .and_then(|_| task.run())
    }

    fn frame(&self) -> &CallFrame {
        let index = self.frames.len() - 1;
        &self.frames[index]
    }

    fn frame_mut(&mut self) -> &mut CallFrame {
        let index = self.frames.len() - 1;
        &mut self.frames[index]
    }

    fn frames_push(&mut self, frame: CallFrame) -> Result<usize, RuntimeError> {
        let frame_count = self.frames.len();
        if frame_count == VM_FRAMES_MAX {
            return Err(RuntimeError::StackOverflow);
        }
        self.frames.push(frame);
        Ok(frame_count)
    }

    fn frames_pop(&mut self) -> CallFrame {
        self.frames.pop().expect("Stack empty.")
    }

    fn stack_push(&mut self, value: Value) -> Result<(), RuntimeError> {
        let stack_size = self.stack.len();
        if stack_size == VM_STACK_SIZE {
            return Err(RuntimeError::StackOverflow);
        }
        self.stack.push(value);
        Ok(())
    }

    fn stack_pop(&mut self) -> Value {
        self.stack.pop().expect("Stack empty.")
    }

    fn stack_top(&self, n: usize) -> &Value {
        let index = self.stack.len() - n - 1;
        &self.stack[index]
    }

    fn stack_top_mut(&mut self, n: usize) -> &mut Value {
        let index = self.stack.len() - n - 1;
        &mut self.stack[index]
    }

    fn stack_remove_top(&mut self, n: usize) {
        self.stack.drain(self.stack.len() - n..);
    }

    fn trace_calls(&self) -> Result<(), RuntimeError> {
        for frame in self.frames.iter().rev() {
            let line = frame.closure.fun.chunk.get_line(frame.ip - 1);
            match &frame.closure.fun.name {
                None => eprintln!("{line} in script."),
                Some(s) => eprintln!("{line} in {s}()."),
            }
        }
        Ok(())
    }

    fn define_native(
        &mut self,
        name: &str,
        arity: u8,
        call: fn(&[Value]) -> Value,
    ) -> Result<(), RuntimeError> {
        let fun_name = self.heap.intern(String::from(name));
        let fun = self.alloc_native_fun(ObjNativeFun { arity, call });
        self.stack_push(Value::Object(fun))?;
        self.globals.insert(fun_name, *self.stack_top(0));
        self.stack_pop();
        Ok(())
    }

    fn alloc_string(&mut self, s: String) -> Object {
        self.gc();
        let s = self.heap.intern(s);
        self.heap.alloc(s, Object::String)
    }

    fn alloc_upvalue(&mut self, upvalue: ObjUpvalue) -> Object {
        self.gc();
        self.heap.alloc(RefCell::new(upvalue), Object::Upvalue)
    }

    fn alloc_closure(&mut self, closure: ObjClosure) -> Object {
        self.gc();
        self.heap.alloc(closure, Object::Closure)
    }

    fn alloc_fun(&mut self, fun: ObjFun) -> Object {
        self.gc();
        self.heap.alloc(fun, Object::Fun)
    }

    fn alloc_native_fun(&mut self, native_fun: ObjNativeFun) -> Object {
        self.gc();
        self.heap.alloc(native_fun, Object::NativeFun)
    }

    fn alloc_class(&mut self, class: ObjClass) -> Object {
        self.gc();
        self.heap.alloc(RefCell::new(class), Object::Class)
    }

    fn alloc_instance(&mut self, instance: ObjInstance) -> Object {
        self.gc();
        self.heap.alloc(RefCell::new(instance), Object::Instance)
    }

    fn alloc_bound_method(&mut self, method: ObjBoundMethod) -> Object {
        self.gc();
        self.heap.alloc(method, Object::BoundMethod)
    }

    fn gc(&mut self) {
        if self.heap.size() <= self.heap.next_gc() {
            return;
        }

        #[cfg(feature = "dbg-heap")]
        let before = {
            println!("-- gc begin");
            self.heap.size()
        };

        self.mark_sweep();

        #[cfg(feature = "dbg-heap")]
        {
            let after = self.heap.size();
            let next = self.heap.next_gc();
            let delta = before.abs_diff(after);
            println!("-- gc end");
            println!("   collected {delta} bytes (from {before} to {after}) next at {next}");
        };
    }

    #[allow(unsafe_code)]
    fn mark_sweep(&mut self) {
        self.mark_roots();
        while let Some(grey_object) = self.grey_objects.pop() {
            grey_object.mark_references(&mut self.grey_objects)
        }
        // SAFETY: We make sure that the sweep step has correctly mark all reachable objects, so
        // sweep can be run safely.
        unsafe { self.heap.sweep() };
    }

    fn mark_roots(&mut self) {
        self.grey_objects.clear();
        for value in &self.stack {
            if let Value::Object(o) = value {
                o.mark(&mut self.grey_objects);
            }
        }
        for frame in &self.frames {
            if frame.closure.mark() {
                self.grey_objects.push(Object::Closure(frame.closure));
            }
        }
        for upvalue in &self.open_upvalues {
            if upvalue.mark() {
                self.grey_objects.push(Object::Upvalue(*upvalue));
            }
        }
        for value in self.globals.values() {
            if let Value::Object(o) = value {
                o.mark(&mut self.grey_objects);
            }
        }
    }

    #[cfg(feature = "dbg-execution")]
    fn trace_stack(&self) {
        print!("          ");
        for value in self.stack.iter() {
            print!("[ {value} ]");
        }
        println!();
    }
}

fn clock_native(_args: &[Value]) -> Value {
    let start = std::time::SystemTime::now();
    let since_epoch = start
        .duration_since(std::time::UNIX_EPOCH)
        .expect("Time went backwards");
    Value::Number(since_epoch.as_secs_f64())
}

/// A task is the structure responsible for executing a single chunk.
struct Task<'vm> {
    vm: &'vm mut VirtualMachine,
}

impl<'vm> Task<'vm> {
    /// Create a new task given the chunk to be run.
    fn new(vm: &'vm mut VirtualMachine) -> Self {
        Self { vm }
    }

    /// Read the next byte in the stream of bytecode instructions.
    fn read_byte(&mut self) -> Result<u8, RuntimeError> {
        self.vm.frame_mut().read_byte()
    }

    /// Read the next 2 bytes in the stream of bytecode instructions.
    fn read_short(&mut self) -> Result<u16, RuntimeError> {
        self.vm.frame_mut().read_short()
    }

    /// Read the next byte in the stream of bytecode instructions and return the constant at the
    /// index given by the byte.
    fn read_constant(&mut self) -> Result<Value, RuntimeError> {
        self.vm.frame_mut().read_constant()
    }
}

impl<'vm> Task<'vm> {
    fn run(&mut self) -> Result<(), RuntimeError> {
        loop {
            #[cfg(feature = "dbg-execution")]
            {
                self.vm.trace_stack();
                let frame = self.vm.frame();
                disassemble_instruction(&frame.closure.fun.chunk, frame.ip);
            }

            match Opcode::try_from(self.read_byte()?)? {
                Opcode::Const => self.constant()?,
                Opcode::Nil => self.vm.stack_push(Value::Nil)?,
                Opcode::True => self.vm.stack_push(Value::Bool(true))?,
                Opcode::False => self.vm.stack_push(Value::Bool(false))?,
                Opcode::Pop => {
                    self.vm.stack_pop();
                }
                Opcode::GetLocal => self.get_local()?,
                Opcode::SetLocal => self.set_local()?,
                Opcode::GetGlobal => self.get_global()?,
                Opcode::SetGlobal => self.set_global()?,
                Opcode::DefineGlobal => self.defined_global()?,
                Opcode::GetUpvalue => self.get_upvalue()?,
                Opcode::SetUpvalue => self.set_upvalue()?,
                Opcode::GetProperty => self.get_property()?,
                Opcode::SetProperty => self.set_property()?,
                Opcode::GetSuper => self.get_super()?,
                Opcode::NE => self.ne()?,
                Opcode::EQ => self.eq()?,
                Opcode::GT => self.gt()?,
                Opcode::GE => self.ge()?,
                Opcode::LT => self.lt()?,
                Opcode::LE => self.le()?,
                Opcode::Add => self.add()?,
                Opcode::Sub => self.sub()?,
                Opcode::Mul => self.mul()?,
                Opcode::Div => self.div()?,
                Opcode::Not => self.not()?,
                Opcode::Neg => self.neg()?,
                Opcode::Print => self.print()?,
                Opcode::Jump => self.jump(JumpDirection::Forward)?,
                Opcode::JumpIfTrue => self.jump_if_true()?,
                Opcode::JumpIfFalse => self.jump_if_false()?,
                Opcode::Loop => self.jump(JumpDirection::Backward)?,
                Opcode::Call => self.call()?,
                Opcode::Invoke => self.invoke()?,
                Opcode::SuperInvoke => self.super_invoke()?,
                Opcode::Closure => self.closure()?,
                Opcode::CloseUpvalue => self.close_upvalue()?,
                Opcode::Ret => {
                    if self.ret()? {
                        break;
                    }
                }
                Opcode::Class => self.class()?,
                Opcode::Inherit => self.inherit()?,
                Opcode::Method => self.method()?,
            }
        }
        Ok(())
    }

    fn super_invoke(&mut self) -> Result<(), RuntimeError> {
        let method_ref = self.read_constant()?.as_object()?;
        let method = method_ref.as_string()?;
        let argc = self.read_byte()?;

        let superclass_ref = self.vm.stack_pop().as_object()?;
        self.invoke_from_class(*superclass_ref.as_class()?, method, argc)?;
        Ok(())
    }

    fn invoke(&mut self) -> Result<(), RuntimeError> {
        let method_ref = self.read_constant()?.as_object()?;
        let method = method_ref.as_string()?;
        let argc = self.read_byte()?;

        let receiver = self.vm.stack_top(argc as usize);
        let instance_ref = receiver
            .as_object()
            .map_err(|_| RuntimeError::InvalidMethodInvocation)?;
        let instance = instance_ref
            .as_instance()
            .map_err(|_| RuntimeError::InvalidMethodInvocation)?;

        if let Some(field) = instance.borrow().fields.get(&***method) {
            *self.vm.stack_top_mut(argc as usize) = *field;
            self.call_value(*field, argc)?;
        } else {
            self.invoke_from_class(instance.borrow().class, method, argc)?;
        }

        Ok(())
    }

    fn invoke_from_class(
        &mut self,
        class: RefClass,
        name: &str,
        argc: u8,
    ) -> Result<(), RuntimeError> {
        let method = class
            .borrow()
            .methods
            .get(name)
            .copied()
            .ok_or_else(|| RuntimeError::UndefinedProperty(name.to_string()))?;
        self.call_closure(method, argc)?;
        Ok(())
    }

    // Bind a method to a class definition. At this moment, a closure object should be the top most
    // item in the stack, and a class definition object should be the second top most item.
    fn method(&mut self) -> Result<(), RuntimeError> {
        let name_ref = self.read_constant()?.as_object()?;
        let name = name_ref.as_string()?;

        let closure_ref = self.vm.stack_pop().as_object()?;
        let closure = closure_ref.as_closure()?;

        let class_ref = self.vm.stack_top(0).as_object()?;
        let class = class_ref.as_class()?;

        class.borrow_mut().methods.insert(Rc::clone(name), *closure);
        Ok(())
    }

    fn bind_method(&mut self, class: RefClass, name: &str) -> Result<bool, RuntimeError> {
        match class.borrow().methods.get(name) {
            Some(method) => {
                let bound = self.vm.alloc_bound_method(ObjBoundMethod {
                    receiver: *self.vm.stack_top(0),
                    method: *method,
                });
                self.vm.stack_pop();
                self.vm.stack_push(Value::Object(bound))?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    fn get_property(&mut self) -> Result<(), RuntimeError> {
        let instance_obj = self
            .vm
            .stack_top(0)
            .as_object()
            .map_err(|_| RuntimeError::ObjectHasNoProperty)?;
        let instance = instance_obj
            .as_instance()
            .map_err(|_| RuntimeError::ObjectHasNoProperty)?;

        let name_obj = self.read_constant()?.as_object()?;
        let name = name_obj.as_string()?;

        let instance = instance.borrow();
        if let Some(value) = instance.fields.get(&***name) {
            self.vm.stack_pop();
            self.vm.stack_push(*value)?;
            Ok(())
        } else if self.bind_method(instance.class, name)? {
            Ok(())
        } else {
            Err(RuntimeError::UndefinedProperty(name.to_string()))
        }
    }

    fn set_property(&mut self) -> Result<(), RuntimeError> {
        let value = self.vm.stack_pop();
        let instance_obj = self
            .vm
            .stack_top(0)
            .as_object()
            .map_err(|_| RuntimeError::ObjectHasNoField)?;
        let instance = instance_obj
            .as_instance()
            .map_err(|_| RuntimeError::ObjectHasNoField)?;

        let name_obj = self.read_constant()?.as_object()?;
        let name = name_obj.as_string()?;

        instance.borrow_mut().fields.insert(Rc::clone(name), value);
        self.vm.stack_pop();
        self.vm.stack_push(value)?;
        Ok(())
    }

    fn get_super(&mut self) -> Result<(), RuntimeError> {
        let name_ref = self.read_constant()?.as_object()?;
        let name = name_ref.as_string()?;
        let superclass = self.vm.stack_pop().as_object()?;
        if !self.bind_method(*superclass.as_class()?, name)? {
            return Err(RuntimeError::UndefinedProperty(name.to_string()));
        }
        Ok(())
    }

    fn class(&mut self) -> Result<(), RuntimeError> {
        let name_ref = self.read_constant()?.as_object()?;
        let name = name_ref.as_string()?;
        let class = self.vm.alloc_class(ObjClass::new(Rc::clone(name)));
        self.vm.stack_push(Value::Object(class))?;
        Ok(())
    }

    fn inherit(&mut self) -> Result<(), RuntimeError> {
        let superclass_ref = self
            .vm
            .stack_top(1)
            .as_object()
            .map_err(|_| RuntimeError::InvalidSuperclass)?;
        let superclass = superclass_ref
            .as_class()
            .map_err(|_| RuntimeError::InvalidSuperclass)?;

        let subclass_ref = self.vm.stack_top(0).as_object()?;
        let subclass = subclass_ref.as_class()?;

        for (method_name, method) in &superclass.borrow().methods {
            subclass
                .borrow_mut()
                .methods
                .insert(Rc::clone(method_name), *method);
        }

        self.vm.stack_pop();
        Ok(())
    }

    /// Get the value of the variable capture by an upvalue.
    fn get_upvalue(&mut self) -> Result<(), RuntimeError> {
        let upvalue_slot = self.read_byte()?;
        let upvalue = self.vm.frame().closure.upvalues[upvalue_slot as usize];
        match *upvalue.borrow() {
            // Value is on the stack.
            ObjUpvalue::Open(stack_slot) => {
                let value = self.vm.stack[stack_slot];
                self.vm.stack_push(value)?;
            }
            // Value is on the heap.
            ObjUpvalue::Closed(value) => {
                self.vm.stack_push(value)?;
            }
        }
        Ok(())
    }

    /// Set the value of the variable capture by an upvalue.
    fn set_upvalue(&mut self) -> Result<(), RuntimeError> {
        let upvalue_slot = self.read_byte()?;
        let value = *self.vm.stack_top(0);
        let stack_slot = {
            let closure = self.vm.frame_mut().closure;
            let mut upvalue = closure.upvalues[upvalue_slot as usize].borrow_mut();
            match upvalue.deref_mut() {
                // Value is on the stack.
                ObjUpvalue::Open(stack_slot) => Some(*stack_slot),
                // Value is on the heap.
                ObjUpvalue::Closed(v) => {
                    *v = value;
                    None
                }
            }
        };
        if let Some(slot) = stack_slot {
            self.vm.stack[slot] = value;
        }
        Ok(())
    }

    fn closure(&mut self) -> Result<(), RuntimeError> {
        let fun_ref = self.read_constant()?.as_object()?;
        let fun = fun_ref.as_fun()?;

        let upvalue_count = fun.upvalue_count as usize;
        let mut upvalues = Vec::with_capacity(upvalue_count);
        for _ in 0..upvalue_count {
            let is_local = self.read_byte()? == 1;
            let index = self.read_byte()? as usize;
            if is_local {
                upvalues.push(self.capture_upvalue(self.vm.frame().slot + index)?);
            } else {
                upvalues.push(self.vm.frame().closure.upvalues[index]);
            }
        }

        let closure = self.vm.alloc_closure(ObjClosure {
            fun: *fun,
            upvalues,
        });
        self.vm.stack_push(Value::Object(closure))?;

        Ok(())
    }

    fn close_upvalue(&mut self) -> Result<(), RuntimeError> {
        self.close_upvalues(self.vm.stack.len() - 1)?;
        self.vm.stack_pop();
        Ok(())
    }

    /// Create an open upvalue capturing the variable at the stack index given by `location`.
    fn capture_upvalue(&mut self, location: usize) -> Result<RefUpvalue, RuntimeError> {
        // Check if there's an existing open upvalues that references the same stack slot. We want
        // to close over a variable not a value. Thus, closures can shared data through the same
        // captured variable.
        for upvalue in self.vm.open_upvalues.iter() {
            if let ObjUpvalue::Open(loc) = *upvalue.borrow() {
                if loc == location {
                    return Ok(*upvalue);
                }
            }
        }
        // Make a new open upvalue.
        let upvalue = self.vm.alloc_upvalue(ObjUpvalue::Open(location));
        self.vm.open_upvalues.push(*upvalue.as_upvalue()?);
        Ok(*upvalue.as_upvalue()?)
    }

    // Close all upvalues whose referenced stack slot went out of scope. Here, `last` is the lowest
    // stack slot that went out of scope.
    fn close_upvalues(&mut self, last: usize) -> Result<(), RuntimeError> {
        for upvalue in &self.vm.open_upvalues {
            // Check if we reference a slot that went out of scope.
            let mut upvalue = upvalue.borrow_mut();
            let stack_slot = match *upvalue {
                ObjUpvalue::Open(slot) if slot >= last => Some(slot),
                _ => None,
            };
            // Hoist the variable up into the upvalue so it can live after the stack frame is pop.
            if let Some(slot) = stack_slot {
                *upvalue = ObjUpvalue::Closed(self.vm.stack[slot]);
            }
        }
        // remove closed upvalues from list of open upvalues
        self.vm
            .open_upvalues
            .retain(|v| matches!(v.borrow().deref(), ObjUpvalue::Open(_)));
        Ok(())
    }

    /// Return from a function call
    fn ret(&mut self) -> Result<bool, RuntimeError> {
        // Get the result of the function.
        let result = self.vm.stack_pop();
        // The compiler didn't emit Opcode::CloseUpvalue at the end of each of the outer most scope
        // that defines the body. That scope contains function parameters and also local variables
        // that need to be closed over.
        self.close_upvalues(self.vm.frame().slot)?;
        let frame = self.vm.frames_pop();
        if self.vm.frames.is_empty() {
            // Have reach the end of the script if there's no stack frame left.
            self.vm.stack_pop();
            return Ok(true);
        }
        // Pop all data related to the stack frame.
        self.vm.stack_remove_top(self.vm.stack.len() - frame.slot);
        // Put the function result on the stack.
        self.vm.stack_push(result)?;
        Ok(false)
    }

    fn call(&mut self) -> Result<(), RuntimeError> {
        let argc = self.read_byte()?;
        let v = self.vm.stack_top(argc as usize);
        self.call_value(*v, argc)?;
        Ok(())
    }

    fn call_value(&mut self, callee: Value, argc: u8) -> Result<(), RuntimeError> {
        match callee {
            Value::Object(o) => self.call_object(o, argc),
            _ => Err(RuntimeError::InvalidCallee),
        }
    }

    fn call_object(&mut self, callee: Object, argc: u8) -> Result<(), RuntimeError> {
        match &callee {
            Object::Closure(c) => self.call_closure(*c, argc),
            Object::NativeFun(f) => self.call_native(*f, argc),
            Object::Class(c) => self.call_class(*c, argc),
            Object::BoundMethod(m) => self.call_bound_method(*m, argc),
            _ => Err(RuntimeError::InvalidCallee),
        }
    }

    fn call_closure(&mut self, callee: RefClosure, argc: u8) -> Result<(), RuntimeError> {
        if argc != callee.fun.arity {
            return Err(RuntimeError::InvalidArgumentsCount {
                arity: callee.fun.arity,
                argc,
            });
        }
        let frame = CallFrame {
            closure: callee,
            ip: 0,
            slot: self.vm.stack.len() - argc as usize - 1,
        };
        self.vm.frames_push(frame)?;
        Ok(())
    }

    fn call_native(&mut self, callee: RefNativeFun, argc: u8) -> Result<(), RuntimeError> {
        if argc != callee.arity {
            return Err(RuntimeError::InvalidArgumentsCount {
                arity: callee.arity,
                argc,
            });
        }
        let sp = self.vm.stack.len();
        let argc = argc as usize;
        let call = callee.call;
        let res = call(&self.vm.stack[sp - argc..]);
        self.vm.stack_remove_top(argc + 1);
        self.vm.stack_push(res)?;
        Ok(())
    }

    fn call_class(&mut self, callee: RefClass, argc: u8) -> Result<(), RuntimeError> {
        // Allocate a new instance and put it on top of the stack.
        let instance = self.vm.alloc_instance(ObjInstance::new(callee));
        *self.vm.stack_top_mut(argc.into()) = Value::Object(instance);
        // Call the 'init' method if there's one
        if let Some(init) = callee.borrow().methods.get("init") {
            self.call_closure(*init, argc)?;
        } else if argc != 0 {
            return Err(RuntimeError::InvalidArgumentsCount { arity: 0, argc });
        }
        Ok(())
    }

    fn call_bound_method(&mut self, callee: RefBoundMethod, argc: u8) -> Result<(), RuntimeError> {
        *self.vm.stack_top_mut(argc as usize) = callee.receiver;
        self.call_closure(callee.method, argc)?;
        Ok(())
    }

    fn jump(&mut self, direction: JumpDirection) -> Result<(), RuntimeError> {
        let offset = self.read_short()?;
        let frame = self.vm.frame_mut();
        match direction {
            JumpDirection::Forward => frame.ip += offset as usize,
            JumpDirection::Backward => frame.ip -= offset as usize,
        }
        Ok(())
    }

    fn jump_if_true(&mut self) -> Result<(), RuntimeError> {
        let offset = self.read_short()?;
        let val = self.vm.stack_top(0);
        if val.is_truthy() {
            self.vm.frame_mut().ip += offset as usize;
        }
        Ok(())
    }

    fn jump_if_false(&mut self) -> Result<(), RuntimeError> {
        let offset = self.read_short()?;
        let val = self.vm.stack_top(0);
        if val.is_falsey() {
            self.vm.frame_mut().ip += offset as usize;
        }
        Ok(())
    }

    /// Get a local variable.
    fn get_local(&mut self) -> Result<(), RuntimeError> {
        let slot = self.read_byte()? as usize;
        let frame_slot = self.vm.frame().slot;
        self.vm.stack_push(self.vm.stack[frame_slot + slot])?;
        Ok(())
    }

    /// Set a local variable.
    fn set_local(&mut self) -> Result<(), RuntimeError> {
        let slot = self.read_byte()? as usize;
        let frame_slot = self.vm.frame().slot;
        self.vm.stack[frame_slot + slot] = *self.vm.stack_top(0);
        Ok(())
    }

    /// Get a global variable or return a runtime error if it was not found.
    fn get_global(&mut self) -> Result<(), RuntimeError> {
        let name_ref = self.read_constant()?.as_object()?;
        let name = name_ref.as_string()?;
        let value = self
            .vm
            .globals
            .get(&***name)
            .ok_or_else(|| RuntimeError::UndefinedVariable(name.to_string()))?;
        self.vm.stack_push(*value)?;
        Ok(())
    }

    /// Set a global variable or return a runtime error if it was not found.
    fn set_global(&mut self) -> Result<(), RuntimeError> {
        let name_ref = self.read_constant()?.as_object()?;
        let name = name_ref.as_string()?;
        let value = self.vm.stack_top(0);
        if !self.vm.globals.contains_key(&***name) {
            return Err(RuntimeError::UndefinedVariable(name.to_string()));
        }
        self.vm.globals.insert(Rc::clone(name), *value);
        Ok(())
    }

    /// Declare a variable with some initial value.
    fn defined_global(&mut self) -> Result<(), RuntimeError> {
        let name_ref = self.read_constant()?.as_object()?;
        let name = name_ref.as_string()?;
        let value = self.vm.stack_pop();
        self.vm.globals.insert(Rc::clone(name), value);
        Ok(())
    }

    /// Read the constant id from the next byte and load the constant with the found id.
    fn constant(&mut self) -> Result<(), RuntimeError> {
        let constant = self.read_constant()?;
        self.vm.stack_push(constant)?;
        Ok(())
    }

    fn ne(&mut self) -> Result<(), RuntimeError> {
        let rhs = self.vm.stack_pop();
        let lhs = self.vm.stack_top_mut(0);
        *lhs = Value::Bool((*lhs).ne(&rhs));
        Ok(())
    }

    fn eq(&mut self) -> Result<(), RuntimeError> {
        let rhs = self.vm.stack_pop();
        let lhs = self.vm.stack_top_mut(0);
        *lhs = Value::Bool((*lhs).eq(&rhs));
        Ok(())
    }

    fn gt(&mut self) -> Result<(), RuntimeError> {
        let rhs = self.vm.stack_pop();
        let lhs = self.vm.stack_top_mut(0);
        *lhs = Value::Bool((*lhs).gt(&rhs)?);
        Ok(())
    }

    fn ge(&mut self) -> Result<(), RuntimeError> {
        let rhs = self.vm.stack_pop();
        let lhs = self.vm.stack_top_mut(0);
        *lhs = Value::Bool((*lhs).ge(&rhs)?);
        Ok(())
    }

    fn lt(&mut self) -> Result<(), RuntimeError> {
        let rhs = self.vm.stack_pop();
        let lhs = self.vm.stack_top_mut(0);
        *lhs = Value::Bool((*lhs).lt(&rhs)?);
        Ok(())
    }

    fn le(&mut self) -> Result<(), RuntimeError> {
        let rhs = self.vm.stack_pop();
        let lhs = self.vm.stack_top_mut(0);
        *lhs = Value::Bool((*lhs).le(&rhs)?);
        Ok(())
    }

    fn add(&mut self) -> Result<(), RuntimeError> {
        // The peek the first 2 items on the stack instead of pop so the GC can see them and won't
        // deaalocate the objects when we allocate a new object for the result.
        let rhs = self.vm.stack_top(0);
        let lhs = self.vm.stack_top(1);
        let res = match (*lhs, rhs) {
            // Operations on objects might allocate a new one.
            (Value::Object(o1), Value::Object(o2)) => match (o1, o2) {
                (Object::String(s1), Object::String(s2)) => {
                    let mut s = String::with_capacity(s1.len() + s1.len());
                    s.push_str(s1.as_ref());
                    s.push_str(s2.as_ref());
                    Value::Object(self.vm.alloc_string(s))
                }
                _ => {
                    return Err(RuntimeError::Value(
                        ValueError::BinaryOperandsMustBeNumbersOrStrings,
                    ))
                }
            },
            // Non-objects can used the `ops::Add` implementation for `Value`
            _ => lhs.add(rhs)?,
        };
        // Remove the RHS.
        self.vm.stack_pop();
        // Update the LHS.
        *self.vm.stack_top_mut(0) = res;
        Ok(())
    }

    fn sub(&mut self) -> Result<(), RuntimeError> {
        let rhs = self.vm.stack_pop();
        let lhs = self.vm.stack_top_mut(0);
        *lhs = lhs.sub(&rhs)?;
        Ok(())
    }

    fn mul(&mut self) -> Result<(), RuntimeError> {
        let rhs = self.vm.stack_pop();
        let lhs = self.vm.stack_top_mut(0);
        *lhs = lhs.mul(&rhs)?;
        Ok(())
    }

    fn div(&mut self) -> Result<(), RuntimeError> {
        let rhs = self.vm.stack_pop();
        let lhs = self.vm.stack_top_mut(0);
        *lhs = lhs.div(&rhs)?;
        Ok(())
    }

    fn not(&mut self) -> Result<(), RuntimeError> {
        let v = self.vm.stack_top_mut(0);
        *v = v.not();
        Ok(())
    }

    fn neg(&mut self) -> Result<(), RuntimeError> {
        let v = self.vm.stack_top_mut(0);
        *v = v.neg()?;
        Ok(())
    }

    fn print(&mut self) -> Result<(), RuntimeError> {
        let val = self.vm.stack_pop();
        println!("{val}");
        Ok(())
    }
}

#[derive(Debug)]
struct CallFrame {
    closure: RefClosure,
    ip: usize,
    slot: usize,
}

impl CallFrame {
    /// Read the next byte in the stream of bytecode instructions.
    fn read_byte(&mut self) -> Result<u8, RuntimeError> {
        let byte = self.closure.fun.chunk.instructions[self.ip];
        self.ip += 1;
        Ok(byte)
    }

    /// Read the next 2 bytes in the stream of bytecode instructions.
    fn read_short(&mut self) -> Result<u16, RuntimeError> {
        let hi = self.closure.fun.chunk.instructions[self.ip] as u16;
        let lo = self.closure.fun.chunk.instructions[self.ip + 1] as u16;
        let short = hi << 8 | lo;
        self.ip += 2;
        Ok(short)
    }

    /// Read the next byte in the stream of bytecode instructions and return the constant at the
    /// index given by the byte.
    fn read_constant(&mut self) -> Result<Value, RuntimeError> {
        let constant_id = self.read_byte()? as usize;
        Ok(self.closure.fun.chunk.constants[constant_id])
    }
}

/// An enumeration that determine whether to jump forward or backward along the stream of
/// bytecode instructions.
pub(crate) enum JumpDirection {
    /// Jump forward.
    Forward,
    /// Jump backward.
    Backward,
}
