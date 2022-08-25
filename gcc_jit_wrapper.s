.global gcc_jit_context_compile_to_file_wrapper
.extern gcc_jit_context_compile_to_file
.global gcc_jit_block_end_with_conditional_wrapper
.extern gcc_jit_block_end_with_conditional
.global gcc_jit_block_end_with_void_return_wrapper
.extern gcc_jit_block_end_with_void_return
.global gcc_jit_lvalue_as_rvalue_wrapper
.extern gcc_jit_lvalue_as_rvalue
.global gcc_jit_context_new_function_wrapper
.extern gcc_jit_context_new_function
.global gcc_jit_context_new_call_wrapper
.extern gcc_jit_context_new_call

.intel_syntax

gcc_jit_context_compile_to_file_wrapper:
    mov %r11, 0x20
    sub %rsp, %r11
    call gcc_jit_context_compile_to_file
    mov %r11, 0x20
    add %rsp, %r11
    ret

gcc_jit_block_end_with_conditional_wrapper:
    mov %r11, 0x40
    sub %rsp, %r11
    call gcc_jit_block_end_with_conditional
    mov %r11, 0x40
    add %rsp, %r11
    ret

gcc_jit_block_end_with_void_return_wrapper:
    mov %r11, 0x30
    sub %rsp, %r11
    call gcc_jit_block_end_with_void_return
    mov %r11, 0x30
    add %rsp, %r11
    ret

gcc_jit_lvalue_as_rvalue_wrapper:
    mov %r11, 0x30
    sub %rsp, %r11
    call gcc_jit_lvalue_as_rvalue
    mov %r11, 0x30
    add %rsp, %r11
    ret

gcc_jit_context_new_function_wrapper:
    mov %r11, 0
    push %r11
    mov %r11, [%rsp + 0x10]
    push %r11
    call gcc_jit_context_new_function
    pop %r11
    pop %r11
    ret

gcc_jit_context_new_call_wrapper:
    call gcc_jit_context_new_call
    ret
