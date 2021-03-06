// xfail-fast  (compile-flags unsupported on windows)
// compile-flags:--borrowck=err

fn borrow(_v: &int) {}

fn borrow_from_arg_imm_ref(&&v: ~int) {
    borrow(v); // ERROR unique value in aliasable, mutable location
}

fn borrow_from_arg_mut_ref(&v: ~int) {
    borrow(v); //! ERROR unique value in aliasable, mutable location
}

fn borrow_from_arg_move(-v: ~int) {
    borrow(v);
}

fn borrow_from_arg_copy(+v: ~int) {
    borrow(v);
}

fn borrow_from_arg_val(++v: ~int) {
    borrow(v);
}

fn main() {
}
