/*

# Type inference engine

This is loosely based on standard HM-type inference, but with an
extension to try and accommodate subtyping.  There is nothing
principled about this extension; it's sound---I hope!---but it's a
heuristic, ultimately, and does not guarantee that it finds a valid
typing even if one exists (in fact, there are known scenarios where it
fails, some of which may eventually become problematic).

## Key idea

The main change is that each type variable T is associated with a
lower-bound L and an upper-bound U.  L and U begin as bottom and top,
respectively, but gradually narrow in response to new constraints
being introduced.  When a variable is finally resolved to a concrete
type, it can (theoretically) select any type that is a supertype of L
and a subtype of U.

There are several critical invariants which we maintain:

- the upper-bound of a variable only becomes lower and the lower-bound
  only becomes higher over time;
- the lower-bound L is always a subtype of the upper bound U;
- the lower-bound L and upper-bound U never refer to other type variables,
  but only to types (though those types may contain type variables).

> An aside: if the terms upper- and lower-bound confuse you, think of
> "supertype" and "subtype".  The upper-bound is a "supertype"
> (super=upper in Latin, or something like that anyway) and the lower-bound
> is a "subtype" (sub=lower in Latin).  I find it helps to visualize
> a simple class hierarchy, like Java minus interfaces and
> primitive types.  The class Object is at the root (top) and other
> types lie in between.  The bottom type is then the Null type.
> So the tree looks like:
>
>             Object
>             /    \
>         String   Other
>             \    /
>             (null)
>
> So the upper bound type is the "supertype" and the lower bound is the
> "subtype" (also, super and sub mean upper and lower in Latin, or something
> like that anyway).

## Satisfying constraints

At a primitive level, there is only one form of constraint that the
inference understands: a subtype relation.  So the outside world can
say "make type A a subtype of type B".  If there are variables
involved, the inferencer will adjust their upper- and lower-bounds as
needed to ensure that this relation is satisfied. (We also allow "make
type A equal to type B", but this is translated into "A <: B" and "B
<: A")

As stated above, we always maintain the invariant that type bounds
never refer to other variables.  This keeps the inference relatively
simple, avoiding the scenario of having a kind of graph where we have
to pump constraints along and reach a fixed point, but it does impose
some heuristics in the case where the user is relating two type
variables A <: B.

The key point when relating type variables is that we do not know what
type the variable represents, but we must make some change that will
ensure that, whatever types A and B are resolved to, they are resolved
to types which have a subtype relation.

There are basically two options here:

- we can merge A and B.  Basically we make them the same variable.
  The lower bound of this new variable is LUB(LB(A), LB(B)) and
  the upper bound is GLB(UB(A), UB(B)).

- we can adjust the bounds of A and B.  Because we do not allow
  type variables to appear in each other's bounds, this only works if A
  and B have appropriate bounds.  But if we can ensure that UB(A) <: LB(B),
  then we know that whatever happens A and B will be resolved to types with
  the appropriate relation.

Our current technique is to *try* (transactionally) to relate the
existing bounds of A and B, if there are any (i.e., if `UB(A) != top
&& LB(B) != bot`).  If that succeeds, we're done.  If it fails, then
we merge A and B into same variable.

This is not clearly the correct course.  For example, if `UB(A) !=
top` but `LB(B) == bot`, we could conceivably set `LB(B)` to `UB(A)`
and leave the variables unmerged.  This is sometimes the better
course, it depends on the program.

The main case which fails today that I would like to support is:

    fn foo<T>(x: T, y: T) { ... }

    fn bar() {
        let x: @mut int = @mut 3;
        let y: @int = @3;
        foo(x, y);
    }

In principle, the inferencer ought to find that the parameter `T` to
`foo(x, y)` is `@const int`.  Today, however, it does not; this is
because the type variable `T` is merged with the type variable for
`X`, and thus inherits its UB/LB of `@mut int`.  This leaves no
flexibility for `T` to later adjust to accommodate `@int`.

## Transactional support

Whenever we adjust merge variables or adjust their bounds, we always
keep a record of the old value.  This allows the changes to be undone.

## Regions

I've only talked about type variables here, but region variables
follow the same principle.  They have upper- and lower-bounds.  A
region A is a subregion of a region B if A being valid implies that B
is valid.  This basically corresponds to the block nesting structure:
the regions for outer block scopes are superregions of those for inner
block scopes.

## GLB/LUB

Computing the greatest-lower-bound and least-upper-bound of two
types/regions is generally straightforward except when type variables
are involved. In that case, we follow a similar "try to use the bounds
when possible but otherwise merge the variables" strategy.  In other
words, `GLB(A, B)` where `A` and `B` are variables will often result
in `A` and `B` being merged and the result being `A`.

## Type assignment

We have a notion of assignability which differs somewhat from
subtyping; in particular it may cause region borrowing to occur.  See
the big comment later in this file on Type Assignment for specifics.

# Implementation details

We make use of a trait-like impementation strategy to consolidate
duplicated code between subtypes, GLB, and LUB computations.  See the
section on "Type Combining" below for details.

*/

import std::smallintmap;
import std::smallintmap::smallintmap;
import std::smallintmap::map;
import std::map::hashmap;
import middle::ty;
import middle::ty::{ty_vid, region_vid, vid};
import syntax::ast;
import syntax::ast::{ret_style};
import util::ppaux::{ty_to_str, mt_to_str};
import result::{result, extensions, ok, err, map, map2, iter2};
import ty::type_is_bot;
import driver::session::session;
import util::common::{indent, indenter};

export infer_ctxt;
export new_infer_ctxt;
export mk_subty;
export mk_subr;
export mk_eqty;
export mk_assignty;
export resolve_shallow;
export resolve_deep;
export resolve_deep_var;
export ty_and_region_var_methods;
export compare_tys;
export fixup_err, fixup_err_to_str;

// Extra information needed to perform an assignment that may borrow.
// The `expr_id` is the is of the expression whose type is being
// assigned, and `borrow_scope` is the region scope to use if the
// value should be borrowed.
type assignment = {
    expr_id: ast::node_id,
    borrow_scope: ast::node_id
};

type bound<T:copy> = option<T>;

type bounds<T:copy> = {lb: bound<T>, ub: bound<T>};

enum var_value<V:copy, T:copy> {
    redirect(V),
    bounded(bounds<T>)
}

type vals_and_bindings<V:copy, T:copy> = {
    vals: smallintmap<var_value<V, T>>,
    mut bindings: [(V, var_value<V, T>)]
};

enum infer_ctxt = @{
    tcx: ty::ctxt,
    vb: vals_and_bindings<ty::ty_vid, ty::t>,
    rb: vals_and_bindings<ty::region_vid, ty::region>,

    // For keeping track of existing type/region variables.
    ty_var_counter: @mut uint,
    region_var_counter: @mut uint,
};

enum fixup_err {
    unresolved_ty(ty_vid),
    cyclic_ty(ty_vid),
    unresolved_region(region_vid),
    cyclic_region(region_vid)
}

fn fixup_err_to_str(f: fixup_err) -> str {
    alt f {
      unresolved_ty(_) { "unconstrained type" }
      cyclic_ty(_) { "cyclic type of infinite size" }
      unresolved_region(_) { "unconstrained region" }
      cyclic_region(_) { "cyclic region" }
    }
}

type ures = result::result<(), ty::type_err>;
type fres<T> = result::result<T, fixup_err>;

fn new_infer_ctxt(tcx: ty::ctxt) -> infer_ctxt {
    infer_ctxt(@{tcx: tcx,
                 vb: {vals: smallintmap::mk(), mut bindings: []},
                 rb: {vals: smallintmap::mk(), mut bindings: []},
                 ty_var_counter: @mut 0u,
                 region_var_counter: @mut 0u})}

fn mk_subty(cx: infer_ctxt, a: ty::t, b: ty::t) -> ures {
    #debug["mk_subty(%s <: %s)", a.to_str(cx), b.to_str(cx)];
    indent {|| cx.commit {|| sub(cx).tys(a, b) } }.to_ures()
}

fn mk_subr(cx: infer_ctxt, a: ty::region, b: ty::region) -> ures {
    #debug["mk_subr(%s <: %s)", a.to_str(cx), b.to_str(cx)];
    indent {|| cx.commit {|| sub(cx).regions(a, b) } }.to_ures()
}

fn mk_eqty(cx: infer_ctxt, a: ty::t, b: ty::t) -> ures {
    #debug["mk_eqty(%s <: %s)", a.to_str(cx), b.to_str(cx)];
    indent {|| cx.commit {|| cx.eq_tys(a, b) } }.to_ures()
}

fn mk_assignty(cx: infer_ctxt, anmnt: assignment,
               a: ty::t, b: ty::t) -> ures {
    #debug["mk_assignty(%? / %s <: %s)",
           anmnt, a.to_str(cx), b.to_str(cx)];
    indent {|| cx.commit {||
        cx.assign_tys(anmnt, a, b)
    } }.to_ures()
}

fn compare_tys(tcx: ty::ctxt, a: ty::t, b: ty::t) -> ures {
    let infcx = new_infer_ctxt(tcx);
    mk_eqty(infcx, a, b)
}

// See comment on the type `resolve_state` below
fn resolve_shallow(cx: infer_ctxt, a: ty::t,
                   force_vars: bool) -> fres<ty::t> {
    resolver(cx, false, force_vars).resolve(a)
}

// See comment on the type `resolve_state` below
fn resolve_deep_var(cx: infer_ctxt, vid: ty_vid,
                    force_vars: bool) -> fres<ty::t> {
    resolver(cx, true, force_vars).resolve(ty::mk_var(cx.tcx, vid))
}

// See comment on the type `resolve_state` below
fn resolve_deep(cx: infer_ctxt, a: ty::t, force_vars: bool) -> fres<ty::t> {
    resolver(cx, true, force_vars).resolve(a)
}

impl methods for ures {
    fn then<T:copy>(f: fn() -> result<T,ty::type_err>)
        -> result<T,ty::type_err> {
        self.chain() {|_i| f() }
    }
}

impl methods<T:copy> for cres<T> {
    fn to_ures() -> ures {
        alt self {
          ok(_v) { ok(()) }
          err(e) { err(e) }
        }
    }

    fn compare(t: T, f: fn() -> ty::type_err) -> cres<T> {
        self.chain {|s|
            if s == t {
                self
            } else {
                err(f())
            }
        }
    }
}

iface to_str {
    fn to_str(cx: infer_ctxt) -> str;
}

impl of to_str for ty::t {
    fn to_str(cx: infer_ctxt) -> str {
        ty_to_str(cx.tcx, self)
    }
}

impl of to_str for ty::mt {
    fn to_str(cx: infer_ctxt) -> str {
        mt_to_str(cx.tcx, self)
    }
}

impl of to_str for ty::region {
    fn to_str(cx: infer_ctxt) -> str {
        util::ppaux::region_to_str(cx.tcx, self)
    }
}

impl<V:copy to_str> of to_str for bound<V> {
    fn to_str(cx: infer_ctxt) -> str {
        alt self {
          some(v) { v.to_str(cx) }
          none { "none " }
        }
    }
}

impl<T:copy to_str> of to_str for bounds<T> {
    fn to_str(cx: infer_ctxt) -> str {
        #fmt["{%s <: %s}",
             self.lb.to_str(cx),
             self.ub.to_str(cx)]
    }
}

impl<V:copy vid, T:copy to_str> of to_str for var_value<V,T> {
    fn to_str(cx: infer_ctxt) -> str {
        alt self {
          redirect(vid) { #fmt("redirect(%s)", vid.to_str()) }
          bounded(bnds) { #fmt("bounded(%s)", bnds.to_str(cx)) }
        }
    }
}

iface st {
    fn sub(infcx: infer_ctxt, b: self) -> ures;
    fn lub(infcx: infer_ctxt, b: self) -> cres<self>;
    fn glb(infcx: infer_ctxt, b: self) -> cres<self>;
}

impl of st for ty::t {
    fn sub(infcx: infer_ctxt, b: ty::t) -> ures {
        sub(infcx).tys(self, b).to_ures()
    }

    fn lub(infcx: infer_ctxt, b: ty::t) -> cres<ty::t> {
        lub(infcx).tys(self, b)
    }

    fn glb(infcx: infer_ctxt, b: ty::t) -> cres<ty::t> {
        glb(infcx).tys(self, b)
    }
}

impl of st for ty::region {
    fn sub(infcx: infer_ctxt, b: ty::region) -> ures {
        sub(infcx).regions(self, b).chain {|_r| ok(()) }
    }

    fn lub(infcx: infer_ctxt, b: ty::region) -> cres<ty::region> {
        lub(infcx).regions(self, b)
    }

    fn glb(infcx: infer_ctxt, b: ty::region) -> cres<ty::region> {
        glb(infcx).regions(self, b)
    }
}

fn uok() -> ures {
    ok(())
}

impl methods for infer_ctxt {
    fn commit<T,E>(f: fn() -> result<T,E>) -> result<T,E> {

        assert self.vb.bindings.len() == 0u;
        assert self.rb.bindings.len() == 0u;

        let r <- self.try(f);

        // TODO---could use a vec::clear() that ran destructors but kept
        // the vec at its currently allocated length
        self.vb.bindings = [];
        self.rb.bindings = [];

        ret r;
    }

    fn try<T,E>(f: fn() -> result<T,E>) -> result<T,E> {

        fn rollback_to<V:copy vid, T:copy>(
            vb: vals_and_bindings<V, T>, len: uint) {

            while vb.bindings.len() != len {
                let (vid, old_v) = vec::pop(vb.bindings);
                vb.vals.insert(vid.to_uint(), old_v);
            }
        }

        let vbl = self.vb.bindings.len();
        let rbl = self.rb.bindings.len();
        #debug["try(vbl=%u, rbl=%u)", vbl, rbl];
        let r <- f();
        alt r {
          result::ok(_) { #debug["try--ok"]; }
          result::err(_) {
            #debug["try--rollback"];
            rollback_to(self.vb, vbl);
            rollback_to(self.rb, rbl);
          }
        }
        ret r;
    }
}

impl ty_and_region_var_methods for infer_ctxt {
    fn next_ty_var_id() -> ty_vid {
        let id = *self.ty_var_counter;
        *self.ty_var_counter += 1u;
        ret ty_vid(id);
    }

    fn next_ty_var() -> ty::t {
        ty::mk_var(self.tcx, self.next_ty_var_id())
    }

    fn next_ty_vars(n: uint) -> [ty::t] {
        vec::from_fn(n) {|_i| self.next_ty_var() }
    }

    fn next_region_var_id() -> region_vid {
        let id = *self.region_var_counter;
        *self.region_var_counter += 1u;
        ret region_vid(id);
    }

    fn next_region_var() -> ty::region {
        ret ty::re_var(self.next_region_var_id());
    }
}

impl unify_methods for infer_ctxt {

    fn set<V:copy vid, T:copy to_str>(
        vb: vals_and_bindings<V, T>, vid: V,
        +new_v: var_value<V, T>) {

        let old_v = vb.vals.get(vid.to_uint());
        vec::push(vb.bindings, (vid, old_v));
        vb.vals.insert(vid.to_uint(), new_v);

        #debug["Updating variable %s from %s to %s",
               vid.to_str(), old_v.to_str(self), new_v.to_str(self)];
    }

    fn get<V:copy vid, T:copy>(
        vb: vals_and_bindings<V, T>, vid: V)
        -> {root: V, bounds:bounds<T>} {

        alt vb.vals.find(vid.to_uint()) {
          none {
            let bnds = {lb: none, ub: none};
            vb.vals.insert(vid.to_uint(), bounded(bnds));
            {root: vid, bounds: bnds}
          }
          some(redirect(vid)) {
            let {root, bounds} = self.get(vb, vid);
            if root != vid {
                vb.vals.insert(vid.to_uint(), redirect(root));
            }
            {root: root, bounds: bounds}
          }
          some(bounded(bounds)) {
            {root: vid, bounds: bounds}
          }
        }
    }

    // Combines the two bounds into a more general bound.
    fn merge_bnd<V:copy to_str>(
        a: bound<V>, b: bound<V>,
        merge_op: fn(V,V) -> cres<V>) -> cres<bound<V>> {

        #debug["merge_bnd(%s,%s)", a.to_str(self), b.to_str(self)];
        let _r = indenter();

        alt (a, b) {
          (none, none) {
            ok(none)
          }
          (some(_), none) {
            ok(a)
          }
          (none, some(_)) {
            ok(b)
          }
          (some(v_a), some(v_b)) {
            merge_op(v_a, v_b).chain {|v|
                ok(some(v))
            }
          }
        }
    }

    fn merge_bnds<V:copy to_str>(
        a: bounds<V>, b: bounds<V>,
        lub: fn(V,V) -> cres<V>,
        glb: fn(V,V) -> cres<V>) -> cres<bounds<V>> {

        let _r = indenter();
        self.merge_bnd(a.ub, b.ub, glb).chain {|ub|
            #debug["glb of ubs %s and %s is %s",
                   a.ub.to_str(self), b.ub.to_str(self),
                   ub.to_str(self)];
            self.merge_bnd(a.lb, b.lb, lub).chain {|lb|
                #debug["lub of lbs %s and %s is %s",
                       a.lb.to_str(self), b.lb.to_str(self),
                       lb.to_str(self)];
                ok({lb: lb, ub: ub})
            }
        }
    }

    // Updates the bounds for the variable `v_id` to be the intersection
    // of `a` and `b`.  That is, the new bounds for `v_id` will be
    // a bounds c such that:
    //    c.ub <: a.ub
    //    c.ub <: b.ub
    //    a.lb <: c.lb
    //    b.lb <: c.lb
    // If this cannot be achieved, the result is failure.
    fn set_var_to_merged_bounds<V:copy vid, T:copy to_str st>(
        vb: vals_and_bindings<V, T>,
        v_id: V, a: bounds<T>, b: bounds<T>) -> ures {

        // Think of the two diamonds, we want to find the
        // intersection.  There are basically four possibilities (you
        // can swap A/B in these pictures):
        //
        //       A         A
        //      / \       / \
        //     / B \     / B \
        //    / / \ \   / / \ \
        //   * *   * * * /   * *
        //    \ \ / /   \   / /
        //     \ B /   / \ / /
        //      \ /   *   \ /
        //       A     \ / A
        //              B

        #debug["merge(%s,%s,%s)",
               v_id.to_str(),
               a.to_str(self),
               b.to_str(self)];

        // First, relate the lower/upper bounds of A and B.
        // Note that these relations *must* hold for us to
        // to be able to merge A and B at all, and relating
        // them explicitly gives the type inferencer more
        // information and helps to produce tighter bounds
        // when necessary.
        indent {||
        self.bnds(a.lb, b.ub).then {||
        self.bnds(b.lb, a.ub).then {||
        self.merge_bnd(a.ub, b.ub, {|x, y| x.glb(self, y)}).chain {|ub|
        self.merge_bnd(a.lb, b.lb, {|x, y| x.lub(self, y)}).chain {|lb|
            let bnds = {lb: lb, ub: ub};
            #debug["merge(%s): bnds=%s",
                   v_id.to_str(),
                   bnds.to_str(self)];

            // the new bounds must themselves
            // be relatable:
            self.bnds(bnds.lb, bnds.ub).then {||
                self.set(vb, v_id, bounded(bnds));
                uok()
            }
        }}}}}
    }

    fn vars<V:copy vid, T:copy to_str st>(
        vb: vals_and_bindings<V, T>,
        a_id: V, b_id: V) -> ures {

        // Need to make sub_id a subtype of sup_id.
        let {root: a_id, bounds: a_bounds} = self.get(vb, a_id);
        let {root: b_id, bounds: b_bounds} = self.get(vb, b_id);

        #debug["vars(%s=%s <: %s=%s)",
               a_id.to_str(), a_bounds.to_str(self),
               b_id.to_str(), b_bounds.to_str(self)];

        if a_id == b_id { ret uok(); }

        // If both A's UB and B's LB have already been bound to types,
        // see if we can make those types subtypes.
        alt (a_bounds.ub, b_bounds.lb) {
          (some(a_ub), some(b_lb)) {
            let r = self.try {|| a_ub.sub(self, b_lb) };
            alt r {
              ok(()) { ret result::ok(()); }
              err(_) { /*fallthrough */ }
            }
          }
          _ { /*fallthrough*/ }
        }

        // For max perf, we should consider the rank here.  But for now,
        // we always make b redirect to a.
        self.set(vb, b_id, redirect(a_id));

        // Otherwise, we need to merge A and B so as to guarantee that
        // A remains a subtype of B.  Actually, there are other options,
        // but that's the route we choose to take.
        self.set_var_to_merged_bounds(vb, a_id, a_bounds, b_bounds).then {||
            uok()
        }
    }

    fn vart<V: copy vid, T: copy to_str st>(
        vb: vals_and_bindings<V, T>,
        a_id: V, b: T) -> ures {

        let {root: a_id, bounds: a_bounds} = self.get(vb, a_id);
        #debug["vart(%s=%s <: %s)",
               a_id.to_str(), a_bounds.to_str(self),
               b.to_str(self)];
        let b_bounds = {lb: none, ub: some(b)};
        self.set_var_to_merged_bounds(vb, a_id, a_bounds, b_bounds)
    }

    fn tvar<V: copy vid, T: copy to_str st>(
        vb: vals_and_bindings<V, T>,
        a: T, b_id: V) -> ures {

        let a_bounds = {lb: some(a), ub: none};
        let {root: b_id, bounds: b_bounds} = self.get(vb, b_id);
        #debug["tvar(%s <: %s=%s)",
               a.to_str(self),
               b_id.to_str(), b_bounds.to_str(self)];
        self.set_var_to_merged_bounds(vb, b_id, a_bounds, b_bounds)
    }

    fn constrs(
        expected: @ty::type_constr,
        actual_constr: @ty::type_constr) -> ures {

        let err_res =
            err(ty::terr_constr_mismatch(expected, actual_constr));

        if expected.node.id != actual_constr.node.id { ret err_res; }
        let expected_arg_len = vec::len(expected.node.args);
        let actual_arg_len = vec::len(actual_constr.node.args);
        if expected_arg_len != actual_arg_len { ret err_res; }
        let mut i = 0u;
        for expected.node.args.each {|a|
            let actual = actual_constr.node.args[i];
            alt a.node {
              ast::carg_base {
                alt actual.node {
                  ast::carg_base { }
                  _ { ret err_res; }
                }
              }
              ast::carg_lit(l) {
                alt actual.node {
                  ast::carg_lit(m) {
                    if l != m { ret err_res; }
                  }
                  _ { ret err_res; }
                }
              }
              ast::carg_ident(p) {
                alt actual.node {
                  ast::carg_ident(q) {
                    if p.idents != q.idents { ret err_res; }
                  }
                  _ { ret err_res; }
                }
              }
            }
            i += 1u;
        }
        ret uok();
    }

    fn bnds<T:copy to_str st>(
        a: bound<T>, b: bound<T>) -> ures {

        #debug("bnds(%s <: %s)", a.to_str(self), b.to_str(self));
        indent {||
            alt (a, b) {
              (none, none) |
              (some(_), none) |
              (none, some(_)) {
                uok()
              }
              (some(t_a), some(t_b)) {
                t_a.sub(self, t_b)
              }
            }
        }
    }

    fn constrvecs(
        as: [@ty::type_constr], bs: [@ty::type_constr]) -> ures {

        if check vec::same_length(as, bs) {
            iter2(as, bs) {|a,b|
                self.constrs(a, b)
            }
        } else {
            err(ty::terr_constr_len(bs.len(), as.len()))
        }
    }

    fn sub_tys(a: ty::t, b: ty::t) -> ures {
        sub(self).tys(a, b).chain {|_t| ok(()) }
    }

    fn sub_regions(a: ty::region, b: ty::region) -> ures {
        sub(self).regions(a, b).chain {|_t| ok(()) }
    }

    fn eq_tys(a: ty::t, b: ty::t) -> ures {
        self.sub_tys(a, b).then {||
            self.sub_tys(b, a)
        }
    }

    fn eq_regions(a: ty::region, b: ty::region) -> ures {
        #debug["eq_regions(%s, %s)",
               a.to_str(self), b.to_str(self)];
        indent {||
            self.sub_regions(a, b).then {||
                self.sub_regions(b, a)
            }
        }
    }
}

// Resolution is the process of removing type variables and replacing
// them with their inferred values.  There are several "modes" for
// resolution.  The first is a shallow resolution: this only resolves
// one layer, but does not resolve any nested variables.  So, for
// example, if we have two variables A and B, and the constraint that
// A <: [B] and B <: int, then shallow resolution on A would yield
// [B].  Deep resolution, on the other hand, would yield [int].
//
// But there is one more knob: the force_vars variable controls the
// behavior in the face of unconstrained variables.  If it is true,
// then unconstrained variables result in an error.

type resolve_state = @{
    infcx: infer_ctxt,
    deep: bool,
    force_vars: bool,
    mut err: option<fixup_err>,
    mut r_seen: [region_vid],
    mut v_seen: [ty_vid]
};

fn resolver(infcx: infer_ctxt, deep: bool, fvars: bool) -> resolve_state {
    @{infcx: infcx,
      deep: deep,
      force_vars: fvars,
      mut err: none,
      mut r_seen: [],
      mut v_seen: []}
}

impl methods for resolve_state {
    fn resolve(typ: ty::t) -> fres<ty::t> {
        self.err = none;

        #debug["Resolving %s (deep=%b, force_vars=%b)",
               ty_to_str(self.infcx.tcx, typ),
               self.deep,
               self.force_vars];

        // n.b. This is a hokey mess because the current fold doesn't
        // allow us to pass back errors in any useful way.

        assert vec::is_empty(self.v_seen) && vec::is_empty(self.r_seen);
        let rty = indent {|| self.resolve1(typ) };
        assert vec::is_empty(self.v_seen) && vec::is_empty(self.r_seen);
        alt self.err {
          none {
            #debug["Resolved to %s (deep=%b, force_vars=%b)",
                   ty_to_str(self.infcx.tcx, rty),
                   self.deep,
                   self.force_vars];
            ret ok(rty);
          }
          some(e) { ret err(e); }
        }
    }

    fn resolve1(typ: ty::t) -> ty::t {
        #debug("Resolve1(%s)", typ.to_str(self.infcx));
        indent(fn&() -> ty::t {
            if !ty::type_needs_infer(typ) { ret typ; }

            alt ty::get(typ).struct {
              ty::ty_var(vid) {
                self.resolve_ty_var(vid)
              }
              _ if !ty::type_has_regions(typ) && !self.deep {
                typ
              }
              _ {
                ty::fold_regions_and_ty(
                    self.infcx.tcx, typ,
                    { |r| self.resolve_region(r) },
                    { |t| self.resolve_if_deep(t) },
                    { |t| self.resolve_if_deep(t) })
              }
            }
        })
    }

    fn resolve_if_deep(typ: ty::t) -> ty::t {
        #debug("Resolve_if_deep(%s)", typ.to_str(self.infcx));
        if !self.deep {typ} else {self.resolve1(typ)}
    }

    fn resolve_region(orig: ty::region) -> ty::region {
        #debug("Resolve_region(%s)", orig.to_str(self.infcx));
        alt orig {
          ty::re_var(rid) { self.resolve_region_var(rid) }
          _ { orig }
        }
    }

    fn resolve_region_var(rid: region_vid) -> ty::region {
        if vec::contains(self.r_seen, rid) {
            self.err = some(cyclic_region(rid));
            ret ty::re_var(rid);
        } else {
            vec::push(self.r_seen, rid);
            let {root:_, bounds} = self.infcx.get(self.infcx.rb, rid);
            let r1 = alt bounds {
              { ub:_, lb:some(t) } { self.resolve_region(t) }
              { ub:some(t), lb:_ } { self.resolve_region(t) }
              { ub:none, lb:none } {
                if self.force_vars {
                    self.err = some(unresolved_region(rid));
                }
                ty::re_var(rid)
              }
            };
            vec::pop(self.r_seen);
            ret r1;
        }
    }

    fn resolve_ty_var(vid: ty_vid) -> ty::t {
        if vec::contains(self.v_seen, vid) {
            self.err = some(cyclic_ty(vid));
            ret ty::mk_var(self.infcx.tcx, vid);
        } else {
            vec::push(self.v_seen, vid);
            let tcx = self.infcx.tcx;

            // Nonobvious: prefer the most specific type
            // (i.e., the lower bound) to the more general
            // one.  More general types in Rust (e.g., fn())
            // tend to carry more restrictions or higher
            // perf. penalties, so it pays to know more.

            let {root:_, bounds} = self.infcx.get(self.infcx.vb, vid);
            let t1 = alt bounds {
              { ub:_, lb:some(t) } if !type_is_bot(t) { self.resolve1(t) }
              { ub:some(t), lb:_ } { self.resolve1(t) }
              { ub:_, lb:some(t) } { self.resolve1(t) }
              { ub:none, lb:none } {
                if self.force_vars {
                    self.err = some(unresolved_ty(vid));
                }
                ty::mk_var(tcx, vid)
              }
            };
            vec::pop(self.v_seen);
            ret t1;
        }
    }
}

// ______________________________________________________________________
// Type assignment
//
// True if rvalues of type `a` can be assigned to lvalues of type `b`.
// This may cause borrowing to the region scope enclosing `a_node_id`.
//
// The strategy here is somewhat non-obvious.  The problem is
// that the constraint we wish to contend with is not a subtyping
// constraint.  Currently, for variables, we only track what it
// must be a subtype of, not what types it must be assignable to
// (or from).  Possibly, we should track that, but I leave that
// refactoring for another day.
//
// Instead, we look at each variable involved and try to extract
// *some* sort of bound.  Typically, the type a is the argument
// supplied to a call; it typically has a *lower bound* (which
// comes from having been assigned a value).  What we'd actually
// *like* here is an upper-bound, but we generally don't have
// one.  The type b is the expected type and it typically has a
// lower-bound too, which is good.
//
// The way we deal with the fact that we often don't have the
// bounds we need is to be a bit careful.  We try to get *some*
// bound from each side, preferring the upper from a and the
// lower from b.  If we fail to get a bound from both sides, then
// we just fall back to requiring that a <: b.
//
// Assuming we have a bound from both sides, we will then examine
// these bounds and see if they have the form (@MT_a, &rb.MT_b)
// (resp. ~MT_a).  If they do not, we fall back to subtyping.
//
// If they *do*, then we know that the two types could never be
// subtypes of one another.  We will then construct a type @MT_b and
// ensure that type a is a subtype of that.  This allows for the
// possibility of assigning from a type like (say) @[mut T1] to a type
// &[const T2] where T1 <: T2.  Basically we would require that @[mut
// T1] <: @[const T2].  Next we require that the region for the
// enclosing scope be a superregion of the region r.  These two checks
// together guarantee that the type A would be a subtype of the type B
// if the @ were converted to a region r.
//
// You might wonder why we don't just make the type &e.MT_a where e is
// the enclosing region and check that &e.MT_a <: B.  The reason is
// that the type @MT_a is (generally) just a *lower-bound*, so this
// would be imposing @MT_a also as the upper-bound on type A.  But
// this upper-bound might be stricter than what is truly needed.

impl assignment for infer_ctxt {
    fn assign_tys(anmnt: assignment, a: ty::t, b: ty::t) -> ures {

        fn select(fst: option<ty::t>, snd: option<ty::t>) -> option<ty::t> {
            alt fst {
              some(t) { some(t) }
              none {
                alt snd {
                  some(t) { some(t) }
                  none { none }
                }
              }
            }
        }

        #debug["assign_tys(anmnt=%?, %s -> %s)",
               anmnt, a.to_str(self), b.to_str(self)];
        let _r = indenter();

        alt (ty::get(a).struct, ty::get(b).struct) {
          (ty::ty_bot, _) {
            uok()
          }

          (ty::ty_var(a_id), ty::ty_var(b_id)) {
            let {root:_, bounds: a_bounds} = self.get(self.vb, a_id);
            let {root:_, bounds: b_bounds} = self.get(self.vb, b_id);
            let a_bnd = select(a_bounds.ub, a_bounds.lb);
            let b_bnd = select(b_bounds.lb, b_bounds.ub);
            self.assign_tys_or_sub(anmnt, a, b, a_bnd, b_bnd)
          }

          (ty::ty_var(a_id), _) {
            let {root:_, bounds:a_bounds} = self.get(self.vb, a_id);
            let a_bnd = select(a_bounds.ub, a_bounds.lb);
            self.assign_tys_or_sub(anmnt, a, b, a_bnd, some(b))
          }

          (_, ty::ty_var(b_id)) {
            let {root:_, bounds: b_bounds} = self.get(self.vb, b_id);
            let b_bnd = select(b_bounds.lb, b_bounds.ub);
            self.assign_tys_or_sub(anmnt, a, b, some(a), b_bnd)
          }

          (_, _) {
            self.assign_tys_or_sub(anmnt, a, b, some(a), some(b))
          }
        }
    }

    fn assign_tys_or_sub(
        anmnt: assignment,
        a: ty::t, b: ty::t,
        a_bnd: option<ty::t>, b_bnd: option<ty::t>) -> ures {

        #debug["assign_tys_or_sub(anmnt=%?, %s -> %s, %s -> %s)",
               anmnt, a.to_str(self), b.to_str(self),
               a_bnd.to_str(self), b_bnd.to_str(self)];
        let _r = indenter();

        fn is_borrowable(v: ty::vstore) -> bool {
            alt v {
              ty::vstore_fixed(_) | ty::vstore_uniq | ty::vstore_box { true }
              ty::vstore_slice(_) { false }
            }
        }

        alt (a_bnd, b_bnd) {
          (some(a_bnd), some(b_bnd)) {
            alt (ty::get(a_bnd).struct, ty::get(b_bnd).struct) {
              (ty::ty_box(mt_a), ty::ty_rptr(r_b, mt_b)) {
                let nr_b = ty::mk_box(self.tcx, mt_b);
                self.crosspollinate(anmnt, a, nr_b, r_b)
              }
              (ty::ty_uniq(mt_a), ty::ty_rptr(r_b, mt_b)) {
                let nr_b = ty::mk_uniq(self.tcx, mt_b);
                self.crosspollinate(anmnt, a, nr_b, r_b)
              }
              (ty::ty_estr(vs_a),
               ty::ty_estr(ty::vstore_slice(r_b)))
              if is_borrowable(vs_a) {
                let nr_b = ty::mk_estr(self.tcx, vs_a);
                self.crosspollinate(anmnt, a, nr_b, r_b)
              }
              (ty::ty_str,
               ty::ty_estr(ty::vstore_slice(r_b))) {
                let nr_b = ty::mk_str(self.tcx);
                self.crosspollinate(anmnt, a, nr_b, r_b)
              }

              (ty::ty_evec(mt_a, vs_a),
               ty::ty_evec(mt_b, ty::vstore_slice(r_b)))
              if is_borrowable(vs_a) {
                let nr_b = ty::mk_evec(self.tcx, mt_b, vs_a);
                self.crosspollinate(anmnt, a, nr_b, r_b)
              }
              (ty::ty_vec(mt_a),
               ty::ty_evec(mt_b, ty::vstore_slice(r_b))) {
                let nr_b = ty::mk_vec(self.tcx, mt_b);
                self.crosspollinate(anmnt, a, nr_b, r_b)
              }
              _ {
                self.sub_tys(a, b)
              }
            }
          }
          _ {
            self.sub_tys(a, b)
          }
        }
    }

    fn crosspollinate(anmnt: assignment,
                     a: ty::t,
                     nr_b: ty::t,
                     r_b: ty::region) -> ures {

        #debug["crosspollinate(anmnt=%?, a=%s, nr_b=%s, r_b=%s)",
               anmnt, a.to_str(self), nr_b.to_str(self),
               r_b.to_str(self)];

        indent {||
            self.sub_tys(a, nr_b).then {||
                let r_a = ty::re_scope(anmnt.borrow_scope);
                #debug["anmnt=%?", anmnt];
                sub(self).contraregions(r_a, r_b).chain {|_r|
                    // if successful, add an entry indicating that
                    // borrowing occurred
                    #debug["borrowing expression #%?", anmnt];
                    self.tcx.borrowings.insert(anmnt.expr_id,
                                               anmnt.borrow_scope);
                    uok()
                }
            }
        }
    }
}

// ______________________________________________________________________
// Type combining
//
// There are three type combiners, sub, lub, and glb.  Each implements
// the interface `combine` contains methods for combining two
// instances of various things and yielding a new instance.  These
// combiner methods always yield a `result<T>`---failure is propagated
// upward using `chain()` methods.
//
// There is a lot of common code for these operations, which is
// abstracted out into functions named `super_X()` which take a combiner
// instance as the first parameter.  This would be better implemented
// using traits.  For this system to work properly, you should not
// call the `super_X(foo, ...)` functions directly, but rather call
// `foo.X(...)`.  The implemtation of `X()` can then choose to delegate
// to the `super` routine or to do other things.
//
// In reality, the sub operation is rather different from lub/glb, but
// they are combined into one interface to avoid duplication (they
// used to be separate but there were many bugs because there were two
// copies of most routines.
//
// The differences are:
//
// - when making two things have a sub relationship, the order of the
//   arguments is significant (a <: b) and the return value of the
//   combine functions is largely irrelevant.  The important thing is
//   whether the action succeeds or fails.  If it succeeds, then side
//   effects have been committed into the type variables.
//
// - for GLB/LUB, the order of arguments is not significant (GLB(a,b) ==
//   GLB(b,a)) and the return value is important (it is the GLB).  Of
//   course GLB/LUB may also have side effects.
//
// Contravariance
//
// When you are relating two things which have a contravariant
// relationship, you should use `contratys()` or `contraregions()`,
// rather than inversing the order of arguments!  This is necessary
// because the order of arguments is not relevant for LUB and GLB.  It
// is also useful to track which value is the "expected" value in
// terms of error reporting, although we do not do that properly right
// now.

type cres<T> = result<T,ty::type_err>;

iface combine {
    fn infcx() -> infer_ctxt;
    fn tag() -> str;

    fn mts(a: ty::mt, b: ty::mt) -> cres<ty::mt>;
    fn contratys(a: ty::t, b: ty::t) -> cres<ty::t>;
    fn tys(a: ty::t, b: ty::t) -> cres<ty::t>;
    fn tps(as: [ty::t], bs: [ty::t]) -> cres<[ty::t]>;
    fn self_tys(a: option<ty::t>, b: option<ty::t>) -> cres<option<ty::t>>;
    fn substs(as: ty::substs, bs: ty::substs) -> cres<ty::substs>;
    fn fns(a: ty::fn_ty, b: ty::fn_ty) -> cres<ty::fn_ty>;
    fn flds(a: ty::field, b: ty::field) -> cres<ty::field>;
    fn modes(a: ast::mode, b: ast::mode) -> cres<ast::mode>;
    fn args(a: ty::arg, b: ty::arg) -> cres<ty::arg>;
    fn protos(p1: ast::proto, p2: ast::proto) -> cres<ast::proto>;
    fn ret_styles(r1: ret_style, r2: ret_style) -> cres<ret_style>;
    fn contraregions(a: ty::region, b: ty::region) -> cres<ty::region>;
    fn regions(a: ty::region, b: ty::region) -> cres<ty::region>;
    fn vstores(vk: ty::terr_vstore_kind,
               a: ty::vstore, b: ty::vstore) -> cres<ty::vstore>;
}

enum sub = infer_ctxt;  // "subtype", "subregion" etc
enum lub = infer_ctxt;  // "least upper bound" (common supertype)
enum glb = infer_ctxt;  // "greatest lower bound" (common subtype)

fn super_substs<C:combine>(
    self: C, a: ty::substs, b: ty::substs) -> cres<ty::substs> {

    fn eq_opt_regions(infcx: infer_ctxt,
                      a: option<ty::region>,
                      b: option<ty::region>) -> cres<option<ty::region>> {
        alt (a, b) {
          (none, none) {
            ok(none)
          }
          (some(a), some(b)) {
            infcx.eq_regions(a, b).then {||
                ok(some(a))
            }
          }
          (_, _) {
            // If these two substitutions are for the same type (and
            // they should be), then the type should either
            // consistenly have a region parameter or not have a
            // region parameter.
            infcx.tcx.sess.bug(
                #fmt["substitution a had opt_region %s and \
                      b had opt_region %s",
                     a.to_str(infcx),
                     b.to_str(infcx)]);
          }
        }
    }

    self.tps(a.tps, b.tps).chain { |tps|
        self.self_tys(a.self_ty, b.self_ty).chain { |self_ty|
            eq_opt_regions(self.infcx(), a.self_r, b.self_r).chain { |self_r|
                ok({self_r: self_r, self_ty: self_ty, tps: tps})
            }
        }
    }
}

fn super_tps<C:combine>(
    self: C, as: [ty::t], bs: [ty::t]) -> cres<[ty::t]> {

    // Note: type parameters are always treated as *invariant*
    // (otherwise the type system would be unsound).  In the
    // future we could allow type parameters to declare a
    // variance.

    if check vec::same_length(as, bs) {
        iter2(as, bs) {|a, b| self.infcx().eq_tys(a, b) }.then {||
            ok(as)
        }
    } else {
        err(ty::terr_ty_param_size(bs.len(), as.len()))
    }
}

fn super_self_tys<C:combine>(
    self: C, a: option<ty::t>, b: option<ty::t>) -> cres<option<ty::t>> {

    // Note: the self type parameter is (currently) always treated as
    // *invariant* (otherwise the type system would be unsound).

    alt (a, b) {
      (none, none) {
        ok(none)
      }
      (some(a), some(b)) {
        self.infcx().eq_tys(a, b).then {||
            ok(some(a))
        }
      }
      (none, some(_)) |
      (some(_), none) {
        // I think it should never happen that we unify two substs and
        // one of them has a self_ty and one doesn't...? I could be
        // wrong about this.
        err(ty::terr_self_substs)
      }
    }
}

fn super_flds<C:combine>(
    self: C, a: ty::field, b: ty::field) -> cres<ty::field> {

    if a.ident == b.ident {
        self.mts(a.mt, b.mt).chain {|mt|
            ok({ident: a.ident, mt: mt})
        }.chain_err {|e|
            err(ty::terr_in_field(@e, a.ident))
        }
    } else {
        err(ty::terr_record_fields(b.ident, a.ident))
    }
}

fn super_modes<C:combine>(
    self: C, a: ast::mode, b: ast::mode)
    -> cres<ast::mode> {

    let tcx = self.infcx().tcx;
    ty::unify_mode(tcx, a, b)
}

fn super_args<C:combine>(
    self: C, a: ty::arg, b: ty::arg)
    -> cres<ty::arg> {

    self.modes(a.mode, b.mode).chain {|m|
        self.contratys(a.ty, b.ty).chain {|t|
            ok({mode: m, ty: t})
        }
    }
}

fn super_vstores<C:combine>(
    self: C, vk: ty::terr_vstore_kind,
    a: ty::vstore, b: ty::vstore) -> cres<ty::vstore> {

    alt (a, b) {
      (ty::vstore_slice(a_r), ty::vstore_slice(b_r)) {
        self.contraregions(a_r, b_r).chain {|r|
            ok(ty::vstore_slice(r))
        }
      }

      _ if a == b {
        ok(a)
      }

      _ {
        err(ty::terr_vstores_differ(vk, b, a))
      }
    }
}

fn super_fns<C:combine>(
    self: C, a_f: ty::fn_ty, b_f: ty::fn_ty) -> cres<ty::fn_ty> {

    fn argvecs<C:combine>(
        self: C, a_args: [ty::arg], b_args: [ty::arg]) -> cres<[ty::arg]> {

        if check vec::same_length(a_args, b_args) {
            map2(a_args, b_args) {|a, b| self.args(a, b) }
        } else {
            err(ty::terr_arg_count)
        }
    }

    self.protos(a_f.proto, b_f.proto).chain {|p|
        self.ret_styles(a_f.ret_style, b_f.ret_style).chain {|rs|
            argvecs(self, a_f.inputs, b_f.inputs).chain {|inputs|
                self.tys(a_f.output, b_f.output).chain {|output|
                    //FIXME self.infcx().constrvecs(a_f.constraints,
                    //FIXME                         b_f.constraints).then {||
                        ok({proto: p,
                            inputs: inputs,
                            output: output,
                            ret_style: rs,
                            constraints: a_f.constraints})
                    //FIXME }
                }
            }
        }
    }
}

fn super_tys<C:combine>(
    self: C, a: ty::t, b: ty::t) -> cres<ty::t> {

    let tcx = self.infcx().tcx;
    alt (ty::get(a).struct, ty::get(b).struct) {
      // The "subtype" ought to be handling cases involving bot or var:
      (ty::ty_bot, _) |
      (_, ty::ty_bot) |
      (ty::ty_var(_), _) |
      (_, ty::ty_var(_)) {
        tcx.sess.bug(
            #fmt["%s: bot and var types should have been handled (%s,%s)",
                 self.tag(),
                 a.to_str(self.infcx()),
                 b.to_str(self.infcx())]);
      }

      (ty::ty_nil, _) |
      (ty::ty_bool, _) |
      (ty::ty_int(_), _) |
      (ty::ty_uint(_), _) |
      (ty::ty_float(_), _) |
      (ty::ty_str, _) {
        let cfg = tcx.sess.targ_cfg;
        if ty::mach_sty(cfg, a) == ty::mach_sty(cfg, b) {
            ok(a)
        } else {
            err(ty::terr_sorts(b, a))
        }
      }

      (ty::ty_param(a_n, _), ty::ty_param(b_n, _)) if a_n == b_n {
        ok(a)
      }

      (ty::ty_enum(a_id, a_substs), ty::ty_enum(b_id, b_substs))
      if a_id == b_id {
        self.substs(a_substs, b_substs).chain {|tps|
            ok(ty::mk_enum(tcx, a_id, tps))
        }
      }

      (ty::ty_iface(a_id, a_substs), ty::ty_iface(b_id, b_substs))
      if a_id == b_id {
        self.substs(a_substs, b_substs).chain {|substs|
            ok(ty::mk_iface(tcx, a_id, substs))
        }
      }

      (ty::ty_class(a_id, a_substs), ty::ty_class(b_id, b_substs))
      if a_id == b_id {
        self.substs(a_substs, b_substs).chain {|substs|
            ok(ty::mk_class(tcx, a_id, substs))
        }
      }

      (ty::ty_box(a_mt), ty::ty_box(b_mt)) {
        self.mts(a_mt, b_mt).chain {|mt|
            ok(ty::mk_box(tcx, mt))
        }
      }

      (ty::ty_uniq(a_mt), ty::ty_uniq(b_mt)) {
        self.mts(a_mt, b_mt).chain {|mt|
            ok(ty::mk_uniq(tcx, mt))
        }
      }

      (ty::ty_vec(a_mt), ty::ty_vec(b_mt)) {
        self.mts(a_mt, b_mt).chain {|mt|
            ok(ty::mk_vec(tcx, mt))
        }
      }

      (ty::ty_ptr(a_mt), ty::ty_ptr(b_mt)) {
        self.mts(a_mt, b_mt).chain {|mt|
            ok(ty::mk_ptr(tcx, mt))
        }
      }

      (ty::ty_rptr(a_r, a_mt), ty::ty_rptr(b_r, b_mt)) {
        self.contraregions(a_r, b_r).chain {|r|
            self.mts(a_mt, b_mt).chain {|mt|
                ok(ty::mk_rptr(tcx, r, mt))
            }
        }
      }

      (ty::ty_evec(a_mt, vs_a), ty::ty_evec(b_mt, vs_b)) {
        self.mts(a_mt, b_mt).chain {|mt|
            self.vstores(ty::terr_vec, vs_a, vs_b).chain {|vs|
                ok(ty::mk_evec(tcx, mt, vs))
            }
        }
      }

      (ty::ty_estr(vs_a), ty::ty_estr(vs_b)) {
        self.vstores(ty::terr_str, vs_a, vs_b).chain {|vs|
            ok(ty::mk_estr(tcx,vs))
        }
      }

      (ty::ty_res(a_id, a_t, a_substs),
       ty::ty_res(b_id, b_t, b_substs))
      if a_id == b_id {
        self.tys(a_t, b_t).chain {|t|
            self.substs(a_substs, b_substs).chain {|substs|
                ok(ty::mk_res(tcx, a_id, t, substs))
            }
        }
      }

      (ty::ty_rec(as), ty::ty_rec(bs)) {
        if check vec::same_length(as, bs) {
            map2(as, bs) {|a,b| self.flds(a, b) }.chain {|flds|
                ok(ty::mk_rec(tcx, flds))
            }
        } else {
            err(ty::terr_record_size(bs.len(), as.len()))
        }
      }

      (ty::ty_tup(as), ty::ty_tup(bs)) {
        if check vec::same_length(as, bs) {
            map2(as, bs) {|a, b| self.tys(a, b) }.chain {|ts|
                ok(ty::mk_tup(tcx, ts))
            }
        } else {
            err(ty::terr_tuple_size(bs.len(), as.len()))
        }
      }

      (ty::ty_fn(a_fty), ty::ty_fn(b_fty)) {
        self.fns(a_fty, b_fty).chain {|fty|
            ok(ty::mk_fn(tcx, fty))
        }
      }

      (ty::ty_constr(a_t, a_constrs), ty::ty_constr(b_t, b_constrs)) {
        self.tys(a_t, b_t).chain {|t|
            self.infcx().constrvecs(a_constrs, b_constrs).then {||
                ok(ty::mk_constr(tcx, t, a_constrs))
            }
        }
      }

      _ { err(ty::terr_sorts(b, a)) }
    }
}

impl of combine for sub {
    fn infcx() -> infer_ctxt { *self }
    fn tag() -> str { "sub" }

    fn lub() -> lub { lub(*self) }

    fn contratys(a: ty::t, b: ty::t) -> cres<ty::t> {
        self.tys(b, a)
    }

    fn contraregions(a: ty::region, b: ty::region) -> cres<ty::region> {
        self.regions(b, a)
    }

    fn regions(a: ty::region, b: ty::region) -> cres<ty::region> {
        #debug["%s.regions(%s, %s)",
               self.tag(),
               a.to_str(self.infcx()),
               b.to_str(self.infcx())];
        indent {||
            alt (a, b) {
              (ty::re_var(a_id), ty::re_var(b_id)) {
                self.infcx().vars(self.rb, a_id, b_id).then {||
                    ok(a)
                }
              }
              (ty::re_var(a_id), _) {
                  self.infcx().vart(self.rb, a_id, b).then {||
                      ok(a)
                  }
              }
              (_, ty::re_var(b_id)) {
                  self.infcx().tvar(self.rb, a, b_id).then {||
                      ok(a)
                  }
              }
              _ {
                self.lub().regions(a, b).compare(b) {||
                    ty::terr_regions_differ(b, a)
                }
              }
            }
        }
    }

    fn mts(a: ty::mt, b: ty::mt) -> cres<ty::mt> {
        #debug("mts(%s <: %s)", a.to_str(*self), b.to_str(*self));

        if a.mutbl != b.mutbl && b.mutbl != ast::m_const {
            ret err(ty::terr_mutability);
        }

        alt b.mutbl {
          ast::m_mutbl {
            // If supertype is mut, subtype must match exactly
            // (i.e., invariant if mut):
            self.infcx().eq_tys(a.ty, b.ty).then {|| ok(a) }
          }
          ast::m_imm | ast::m_const {
            // Otherwise we can be covariant:
            self.tys(a.ty, b.ty).chain {|_t| ok(a) }
          }
        }
    }

    fn protos(a: ast::proto, b: ast::proto) -> cres<ast::proto> {
        self.lub().protos(a, b).compare(b) {||
            ty::terr_proto_mismatch(b, a)
        }
    }

    fn ret_styles(a: ret_style, b: ret_style) -> cres<ret_style> {
        self.lub().ret_styles(a, b).compare(b) {||
            ty::terr_ret_style_mismatch(b, a)
        }
    }

    fn tys(a: ty::t, b: ty::t) -> cres<ty::t> {
        #debug("%s.tys(%s, %s)", self.tag(),
               a.to_str(*self), b.to_str(*self));
        if a == b { ret ok(a); }
        indent {||
            alt (ty::get(a).struct, ty::get(b).struct) {
              (ty::ty_bot, _) {
                ok(a)
              }
              (ty::ty_var(a_id), ty::ty_var(b_id)) {
                self.infcx().vars(self.vb, a_id, b_id).then {|| ok(a) }
              }
              (ty::ty_var(a_id), _) {
                self.infcx().vart(self.vb, a_id, b).then {|| ok(a) }
              }
              (_, ty::ty_var(b_id)) {
                self.infcx().tvar(self.vb, a, b_id).then {|| ok(a) }
              }
              (_, ty::ty_bot) {
                err(ty::terr_sorts(b, a))
              }
              _ {
                super_tys(self, a, b)
              }
            }
        }
    }

    // Traits please:

    fn flds(a: ty::field, b: ty::field) -> cres<ty::field> {
        super_flds(self, a, b)
    }

    fn vstores(vk: ty::terr_vstore_kind,
               a: ty::vstore, b: ty::vstore) -> cres<ty::vstore> {
        super_vstores(self, vk, a, b)
    }

    fn modes(a: ast::mode, b: ast::mode) -> cres<ast::mode> {
        super_modes(self, a, b)
    }

    fn args(a: ty::arg, b: ty::arg) -> cres<ty::arg> {
        super_args(self, a, b)
    }

    fn fns(a: ty::fn_ty, b: ty::fn_ty) -> cres<ty::fn_ty> {
        super_fns(self, a, b)
    }

    fn substs(as: ty::substs, bs: ty::substs) -> cres<ty::substs> {
        super_substs(self, as, bs)
    }

    fn tps(as: [ty::t], bs: [ty::t]) -> cres<[ty::t]> {
        super_tps(self, as, bs)
    }

    fn self_tys(a: option<ty::t>, b: option<ty::t>) -> cres<option<ty::t>> {
        super_self_tys(self, a, b)
    }
}

impl of combine for lub {
    fn infcx() -> infer_ctxt { *self }
    fn tag() -> str { "lub" }

    fn bot_ty(b: ty::t) -> cres<ty::t> { ok(b) }
    fn ty_bot(b: ty::t) -> cres<ty::t> { self.bot_ty(b) } // commutative

    fn mts(a: ty::mt, b: ty::mt) -> cres<ty::mt> {
        let tcx = self.infcx().tcx;

        #debug("%s.mts(%s, %s)",
               self.tag(),
               mt_to_str(tcx, a),
               mt_to_str(tcx, b));

        let m = if a.mutbl == b.mutbl {
            a.mutbl
        } else {
            ast::m_const
        };

        alt m {
          ast::m_imm | ast::m_const {
            self.tys(a.ty, b.ty).chain {|t|
                ok({ty: t, mutbl: m})
            }
          }

          ast::m_mutbl {
            self.infcx().try {||
                self.infcx().eq_tys(a.ty, b.ty).then {||
                    ok({ty: a.ty, mutbl: m})
                }
            }.chain_err {|_e|
                self.tys(a.ty, b.ty).chain {|t|
                    ok({ty: t, mutbl: ast::m_const})
                }
            }
          }
        }
    }

    fn contratys(a: ty::t, b: ty::t) -> cres<ty::t> {
        glb(self.infcx()).tys(a, b)
    }

    fn protos(p1: ast::proto, p2: ast::proto) -> cres<ast::proto> {
        if p1 == ast::proto_bare {
            ok(p2)
        } else if p2 == ast::proto_bare {
            ok(p1)
        } else if p1 == p2 {
            ok(p1)
        } else {
            ok(ast::proto_any)
        }
    }

    fn ret_styles(r1: ret_style, r2: ret_style) -> cres<ret_style> {
        alt (r1, r2) {
          (ast::return_val, _) |
          (_, ast::return_val) {
            ok(ast::return_val)
          }
          (ast::noreturn, ast::noreturn) {
            ok(ast::noreturn)
          }
        }
    }

    fn contraregions(a: ty::region, b: ty::region) -> cres<ty::region> {
        ret glb(self.infcx()).regions(a, b);
    }

    fn regions(a: ty::region, b: ty::region) -> cres<ty::region> {
        #debug["%s.regions(%?, %?)",
               self.tag(),
               a.to_str(self.infcx()),
               b.to_str(self.infcx())];

        indent {||
            alt (a, b) {
              (ty::re_static, _) | (_, ty::re_static) {
                ok(ty::re_static) // nothing lives longer than static
              }

              (ty::re_var(_), _) | (_, ty::re_var(_)) {
                lattice_rvars(self, a, b)
              }

              (f @ ty::re_free(f_id, _), ty::re_scope(s_id)) |
              (ty::re_scope(s_id), f @ ty::re_free(f_id, _)) {
                // A "free" region can be interpreted as "some region
                // at least as big as the block f_id".  So, we can
                // reasonably compare free regions and scopes:
                let rm = self.infcx().tcx.region_map;
                alt region::nearest_common_ancestor(rm, f_id, s_id) {
                  // if the free region's scope `f_id` is bigger than
                  // the scope region `s_id`, then the LUB is the free
                  // region itself:
                  some(r_id) if r_id == f_id { ok(f) }

                  // otherwise, we don't know what the free region is,
                  // so we must conservatively say the LUB is static:
                  _ { ok(ty::re_static) }
                }
              }

              (ty::re_scope(a_id), ty::re_scope(b_id)) {
                // The region corresponding to an outer block is a
                // subtype of the region corresponding to an inner
                // block.
                let rm = self.infcx().tcx.region_map;
                alt region::nearest_common_ancestor(rm, a_id, b_id) {
                  some(r_id) { ok(ty::re_scope(r_id)) }
                  _ { ok(ty::re_static) }
                }
              }

              // For these types, we cannot define any additional
              // relationship:
              (ty::re_free(_, _), ty::re_free(_, _)) |
              (ty::re_bound(_), ty::re_bound(_)) |
              (ty::re_bound(_), ty::re_free(_, _)) |
              (ty::re_bound(_), ty::re_scope(_)) |
              (ty::re_free(_, _), ty::re_bound(_)) |
              (ty::re_scope(_), ty::re_bound(_)) {
                if a == b {
                    ok(a)
                } else {
                    ok(ty::re_static)
                }
              }
            }
        }
    }

    // Traits please:

    fn tys(a: ty::t, b: ty::t) -> cres<ty::t> {
        lattice_tys(self, a, b)
    }

    fn flds(a: ty::field, b: ty::field) -> cres<ty::field> {
        super_flds(self, a, b)
    }

    fn vstores(vk: ty::terr_vstore_kind,
               a: ty::vstore, b: ty::vstore) -> cres<ty::vstore> {
        super_vstores(self, vk, a, b)
    }

    fn modes(a: ast::mode, b: ast::mode) -> cres<ast::mode> {
        super_modes(self, a, b)
    }

    fn args(a: ty::arg, b: ty::arg) -> cres<ty::arg> {
        super_args(self, a, b)
    }

    fn fns(a: ty::fn_ty, b: ty::fn_ty) -> cres<ty::fn_ty> {
        super_fns(self, a, b)
    }

    fn substs(as: ty::substs, bs: ty::substs) -> cres<ty::substs> {
        super_substs(self, as, bs)
    }

    fn tps(as: [ty::t], bs: [ty::t]) -> cres<[ty::t]> {
        super_tps(self, as, bs)
    }

    fn self_tys(a: option<ty::t>, b: option<ty::t>) -> cres<option<ty::t>> {
        super_self_tys(self, a, b)
    }
}

impl of combine for glb {
    fn infcx() -> infer_ctxt { *self }
    fn tag() -> str { "glb" }

    fn mts(a: ty::mt, b: ty::mt) -> cres<ty::mt> {
        let tcx = self.infcx().tcx;

        #debug("%s.mts(%s, %s)",
               self.tag(),
               mt_to_str(tcx, a),
               mt_to_str(tcx, b));

        alt (a.mutbl, b.mutbl) {
          // If one side or both is mut, then the GLB must use
          // the precise type from the mut side.
          (ast::m_mutbl, ast::m_const) {
            sub(*self).tys(a.ty, b.ty).chain {|_t|
                ok({ty: a.ty, mutbl: ast::m_mutbl})
            }
          }
          (ast::m_const, ast::m_mutbl) {
            sub(*self).tys(b.ty, a.ty).chain {|_t|
                ok({ty: b.ty, mutbl: ast::m_mutbl})
            }
          }
          (ast::m_mutbl, ast::m_mutbl) {
            self.infcx().eq_tys(a.ty, b.ty).then {||
                ok({ty: a.ty, mutbl: ast::m_mutbl})
            }
          }

          // If one side or both is immutable, we can use the GLB of
          // both sides but mutbl must be `m_imm`.
          (ast::m_imm, ast::m_const) |
          (ast::m_const, ast::m_imm) |
          (ast::m_imm, ast::m_imm) {
            self.tys(a.ty, b.ty).chain {|t|
                ok({ty: t, mutbl: ast::m_imm})
            }
          }

          // If both sides are const, then we can use GLB of both
          // sides and mutbl of only `m_const`.
          (ast::m_const, ast::m_const) {
            self.tys(a.ty, b.ty).chain {|t|
                ok({ty: t, mutbl: ast::m_const})
            }
          }

          // There is no mutual subtype of these combinations.
          (ast::m_mutbl, ast::m_imm) |
          (ast::m_imm, ast::m_mutbl) {
              err(ty::terr_mutability)
          }
        }
    }

    fn contratys(a: ty::t, b: ty::t) -> cres<ty::t> {
        lub(self.infcx()).tys(a, b)
    }

    fn protos(p1: ast::proto, p2: ast::proto) -> cres<ast::proto> {
        if p1 == ast::proto_any {
            ok(p2)
        } else if p2 == ast::proto_any {
            ok(p1)
        } else if p1 == p2 {
            ok(p1)
        } else {
            ok(ast::proto_bare)
        }
    }

    fn ret_styles(r1: ret_style, r2: ret_style) -> cres<ret_style> {
        alt (r1, r2) {
          (ast::return_val, ast::return_val) {
            ok(ast::return_val)
          }
          (ast::noreturn, _) |
          (_, ast::noreturn) {
            ok(ast::noreturn)
          }
        }
    }

    fn regions(a: ty::region, b: ty::region) -> cres<ty::region> {
        #debug["%s.regions(%?, %?)",
               self.tag(),
               a.to_str(self.infcx()),
               b.to_str(self.infcx())];

        indent {||
            alt (a, b) {
              (ty::re_static, r) | (r, ty::re_static) {
                // static lives longer than everything else
                ok(r)
              }

              (ty::re_var(_), _) | (_, ty::re_var(_)) {
                lattice_rvars(self, a, b)
              }

              (ty::re_free(f_id, _), s @ ty::re_scope(s_id)) |
              (s @ ty::re_scope(s_id), ty::re_free(f_id, _)) {
                // Free region is something "at least as big as
                // `f_id`."  If we find that the scope `f_id` is bigger
                // than the scope `s_id`, then we can say that the GLB
                // is the scope `s_id`.  Otherwise, as we do not know
                // big the free region is precisely, the GLB is undefined.
                let rm = self.infcx().tcx.region_map;
                alt region::nearest_common_ancestor(rm, f_id, s_id) {
                  some(r_id) if r_id == f_id { ok(s) }
                  _ { err(ty::terr_regions_differ(b, a)) }
                }
              }

              (ty::re_scope(a_id), ty::re_scope(b_id)) {
                // We want to generate a region that is contained by both of
                // these: so, if one of these scopes is a subscope of the
                // other, return it.  Otherwise fail.
                let rm = self.infcx().tcx.region_map;
                alt region::nearest_common_ancestor(rm, a_id, b_id) {
                  some(r_id) if a_id == r_id { ok(b) }
                  some(r_id) if b_id == r_id { ok(a) }
                  _ { err(ty::terr_regions_differ(b, a)) }
                }
              }

              // For these types, we cannot define any additional
              // relationship:
              (ty::re_free(_, _), ty::re_free(_, _)) |
              (ty::re_bound(_), ty::re_bound(_)) |
              (ty::re_bound(_), ty::re_free(_, _)) |
              (ty::re_bound(_), ty::re_scope(_)) |
              (ty::re_free(_, _), ty::re_bound(_)) |
              (ty::re_scope(_), ty::re_bound(_)) {
                if a == b {
                    ok(a)
                } else {
                    err(ty::terr_regions_differ(b, a))
                }
              }
            }
        }
    }

    fn contraregions(a: ty::region, b: ty::region) -> cres<ty::region> {
        lub(self.infcx()).regions(a, b)
    }

    fn tys(a: ty::t, b: ty::t) -> cres<ty::t> {
        lattice_tys(self, a, b)
    }

    // Traits please:

    fn flds(a: ty::field, b: ty::field) -> cres<ty::field> {
        super_flds(self, a, b)
    }

    fn vstores(vk: ty::terr_vstore_kind,
               a: ty::vstore, b: ty::vstore) -> cres<ty::vstore> {
        super_vstores(self, vk, a, b)
    }

    fn modes(a: ast::mode, b: ast::mode) -> cres<ast::mode> {
        super_modes(self, a, b)
    }

    fn args(a: ty::arg, b: ty::arg) -> cres<ty::arg> {
        super_args(self, a, b)
    }

    fn fns(a: ty::fn_ty, b: ty::fn_ty) -> cres<ty::fn_ty> {
        super_fns(self, a, b)
    }

    fn substs(as: ty::substs, bs: ty::substs) -> cres<ty::substs> {
        super_substs(self, as, bs)
    }

    fn tps(as: [ty::t], bs: [ty::t]) -> cres<[ty::t]> {
        super_tps(self, as, bs)
    }

    fn self_tys(a: option<ty::t>, b: option<ty::t>) -> cres<option<ty::t>> {
        super_self_tys(self, a, b)
    }
}

// ______________________________________________________________________
// Lattice operations on variables
//
// This is common code used by both LUB and GLB to compute the LUB/GLB
// for pairs of variables or for variables and values.

iface lattice_ops {
    fn bnd<T:copy>(b: bounds<T>) -> option<T>;
    fn with_bnd<T:copy>(b: bounds<T>, t: T) -> bounds<T>;
    fn ty_bot(t: ty::t) -> cres<ty::t>;
}

impl of lattice_ops for lub {
    fn bnd<T:copy>(b: bounds<T>) -> option<T> { b.ub }
    fn with_bnd<T:copy>(b: bounds<T>, t: T) -> bounds<T> {
        {ub: some(t) with b}
    }
    fn ty_bot(t: ty::t) -> cres<ty::t> {
        ok(t)
    }
}

impl of lattice_ops for glb {
    fn bnd<T:copy>(b: bounds<T>) -> option<T> { b.lb }
    fn with_bnd<T:copy>(b: bounds<T>, t: T) -> bounds<T> {
        {lb: some(t) with b}
    }
    fn ty_bot(_t: ty::t) -> cres<ty::t> {
        ok(ty::mk_bot(self.infcx().tcx))
    }
}

fn lattice_tys<L:lattice_ops combine>(
    self: L, a: ty::t, b: ty::t) -> cres<ty::t> {

    #debug("%s.tys(%s, %s)", self.tag(),
           a.to_str(self.infcx()),
           b.to_str(self.infcx()));
    if a == b { ret ok(a); }
    indent {||
        alt (ty::get(a).struct, ty::get(b).struct) {
          (ty::ty_bot, _) { self.ty_bot(b) }
          (_, ty::ty_bot) { self.ty_bot(a) }

          (ty::ty_var(a_id), ty::ty_var(b_id)) {
            lattice_vars(self, self.infcx().vb,
                         a, a_id, b_id,
                         {|x, y| self.tys(x, y) })
          }

          (ty::ty_var(a_id), _) {
            lattice_var_t(self, self.infcx().vb, a_id, b,
                          {|x, y| self.tys(x, y) })
          }

          (_, ty::ty_var(b_id)) {
            lattice_var_t(self, self.infcx().vb, b_id, a,
                          {|x, y| self.tys(x, y) })
          }

          _ {
            super_tys(self, a, b)
          }
        }
    }
}

// Pull out some common code from LUB/GLB for handling region vars:
fn lattice_rvars<L:lattice_ops combine>(
    self: L, a: ty::region, b: ty::region) -> cres<ty::region> {

    alt (a, b) {
      (ty::re_var(a_id), ty::re_var(b_id)) {
        lattice_vars(self, self.infcx().rb,
                     a, a_id, b_id,
                     {|x, y| self.regions(x, y) })
      }

      (ty::re_var(v_id), r) | (r, ty::re_var(v_id)) {
        lattice_var_t(self, self.infcx().rb,
                      v_id, r,
                      {|x, y| self.regions(x, y) })
      }

      _ {
        self.infcx().tcx.sess.bug(
            #fmt["%s: lattice_rvars invoked with a=%s and b=%s, \
                  neither of which are region variables",
                 self.tag(),
                 a.to_str(self.infcx()),
                 b.to_str(self.infcx())]);
      }
    }
}

fn lattice_vars<V:copy vid, T:copy to_str st, L:lattice_ops combine>(
    self: L, vb: vals_and_bindings<V, T>,
    a_t: T, a_vid: V, b_vid: V,
    c_ts: fn(T, T) -> cres<T>) -> cres<T> {

    // The comments in this function are written for LUB and types,
    // but they apply equally well to GLB and regions if you inverse
    // upper/lower/sub/super/etc.

    // Need to find a type that is a supertype of both a and b:
    let {root: a_vid, bounds: a_bounds} = self.infcx().get(vb, a_vid);
    let {root: b_vid, bounds: b_bounds} = self.infcx().get(vb, b_vid);

    #debug["%s.lattice_vars(%s=%s <: %s=%s)",
           self.tag(),
           a_vid.to_str(), a_bounds.to_str(self.infcx()),
           b_vid.to_str(), b_bounds.to_str(self.infcx())];

    if a_vid == b_vid {
        ret ok(a_t);
    }

    // If both A and B have an UB type, then we can just compute the
    // LUB of those types:
    let a_bnd = self.bnd(a_bounds), b_bnd = self.bnd(b_bounds);
    alt (a_bnd, b_bnd) {
      (some(a_ty), some(b_ty)) {
        alt self.infcx().try {|| c_ts(a_ty, b_ty) } {
            ok(t) { ret ok(t); }
            err(_) { /*fallthrough */ }
        }
      }
      _ {/*fallthrough*/}
    }

    // Otherwise, we need to merge A and B into one variable.  We can
    // then use either variable as an upper bound:
    self.infcx().vars(vb, a_vid, b_vid).then {||
        ok(a_t)
    }
}

fn lattice_var_t<V:copy vid, T:copy to_str st, L:lattice_ops combine>(
    self: L, vb: vals_and_bindings<V, T>,
    a_id: V, b: T,
    c_ts: fn(T, T) -> cres<T>) -> cres<T> {

    let {root: a_id, bounds: a_bounds} = self.infcx().get(vb, a_id);

    // The comments in this function are written for LUB, but they
    // apply equally well to GLB if you inverse upper/lower/sub/super/etc.

    #debug["%s.lattice_vart(%s=%s <: %s)",
           self.tag(),
           a_id.to_str(), a_bounds.to_str(self.infcx()),
           b.to_str(self.infcx())];

    alt self.bnd(a_bounds) {
      some(a_bnd) {
        // If a has an upper bound, return it.
        #debug["bnd=some(%s)", a_bnd.to_str(self.infcx())];
        ret c_ts(a_bnd, b);
      }
      none {
        // If a does not have an upper bound, make b the upper bound of a
        // and then return b.
        #debug["bnd=none"];
        let a_bounds = self.with_bnd(a_bounds, b);
        self.infcx().bnds(a_bounds.lb, a_bounds.ub).then {||
            self.infcx().set(vb, a_id, bounded(a_bounds));
            ok(b)
        }
      }
    }
}
