use clippy_utils::source::{snippet_opt, span_starts_with, walk_span_to_context};
use clippy_utils::{higher, in_constant, meets_msrv, msrvs};
use rustc_hir::{Arm, Expr, ExprKind, Local, MatchSource, Pat};
use rustc_lexer::{tokenize, TokenKind};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_middle::lint::in_external_macro;
use rustc_semver::RustcVersion;
use rustc_session::{declare_tool_lint, impl_lint_pass};
use rustc_span::{Span, SpanData, SyntaxContext};

mod collapsible_match;
mod infallible_destructuring_match;
mod manual_map;
mod manual_unwrap_or;
mod match_as_ref;
mod match_bool;
mod match_like_matches;
mod match_on_vec_items;
mod match_ref_pats;
mod match_same_arms;
mod match_single_binding;
mod match_str_case_mismatch;
mod match_wild_enum;
mod match_wild_err_arm;
mod needless_match;
mod overlapping_arms;
mod redundant_pattern_match;
mod rest_pat_in_fully_bound_struct;
mod significant_drop_in_scrutinee;
mod single_match;
mod try_err;
mod wild_in_or_pats;

declare_clippy_lint! {
    /// ### What it does
    /// Checks for matches with a single arm where an `if let`
    /// will usually suffice.
    ///
    /// ### Why is this bad?
    /// Just readability – `if let` nests less than a `match`.
    ///
    /// ### Example
    /// ```rust
    /// # fn bar(stool: &str) {}
    /// # let x = Some("abc");
    /// match x {
    ///     Some(ref foo) => bar(foo),
    ///     _ => (),
    /// }
    /// ```
    ///
    /// Use instead:
    /// ```rust
    /// # fn bar(stool: &str) {}
    /// # let x = Some("abc");
    /// if let Some(ref foo) = x {
    ///     bar(foo);
    /// }
    /// ```
    #[clippy::version = "pre 1.29.0"]
    pub SINGLE_MATCH,
    style,
    "a `match` statement with a single nontrivial arm (i.e., where the other arm is `_ => {}`) instead of `if let`"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for matches with two arms where an `if let else` will
    /// usually suffice.
    ///
    /// ### Why is this bad?
    /// Just readability – `if let` nests less than a `match`.
    ///
    /// ### Known problems
    /// Personal style preferences may differ.
    ///
    /// ### Example
    /// Using `match`:
    ///
    /// ```rust
    /// # fn bar(foo: &usize) {}
    /// # let other_ref: usize = 1;
    /// # let x: Option<&usize> = Some(&1);
    /// match x {
    ///     Some(ref foo) => bar(foo),
    ///     _ => bar(&other_ref),
    /// }
    /// ```
    ///
    /// Using `if let` with `else`:
    ///
    /// ```rust
    /// # fn bar(foo: &usize) {}
    /// # let other_ref: usize = 1;
    /// # let x: Option<&usize> = Some(&1);
    /// if let Some(ref foo) = x {
    ///     bar(foo);
    /// } else {
    ///     bar(&other_ref);
    /// }
    /// ```
    #[clippy::version = "pre 1.29.0"]
    pub SINGLE_MATCH_ELSE,
    pedantic,
    "a `match` statement with two arms where the second arm's pattern is a placeholder instead of a specific match pattern"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for matches where all arms match a reference,
    /// suggesting to remove the reference and deref the matched expression
    /// instead. It also checks for `if let &foo = bar` blocks.
    ///
    /// ### Why is this bad?
    /// It just makes the code less readable. That reference
    /// destructuring adds nothing to the code.
    ///
    /// ### Example
    /// ```rust,ignore
    /// match x {
    ///     &A(ref y) => foo(y),
    ///     &B => bar(),
    ///     _ => frob(&x),
    /// }
    /// ```
    ///
    /// Use instead:
    /// ```rust,ignore
    /// match *x {
    ///     A(ref y) => foo(y),
    ///     B => bar(),
    ///     _ => frob(x),
    /// }
    /// ```
    #[clippy::version = "pre 1.29.0"]
    pub MATCH_REF_PATS,
    style,
    "a `match` or `if let` with all arms prefixed with `&` instead of deref-ing the match expression"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for matches where match expression is a `bool`. It
    /// suggests to replace the expression with an `if...else` block.
    ///
    /// ### Why is this bad?
    /// It makes the code less readable.
    ///
    /// ### Example
    /// ```rust
    /// # fn foo() {}
    /// # fn bar() {}
    /// let condition: bool = true;
    /// match condition {
    ///     true => foo(),
    ///     false => bar(),
    /// }
    /// ```
    /// Use if/else instead:
    /// ```rust
    /// # fn foo() {}
    /// # fn bar() {}
    /// let condition: bool = true;
    /// if condition {
    ///     foo();
    /// } else {
    ///     bar();
    /// }
    /// ```
    #[clippy::version = "pre 1.29.0"]
    pub MATCH_BOOL,
    pedantic,
    "a `match` on a boolean expression instead of an `if..else` block"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for overlapping match arms.
    ///
    /// ### Why is this bad?
    /// It is likely to be an error and if not, makes the code
    /// less obvious.
    ///
    /// ### Example
    /// ```rust
    /// let x = 5;
    /// match x {
    ///     1..=10 => println!("1 ... 10"),
    ///     5..=15 => println!("5 ... 15"),
    ///     _ => (),
    /// }
    /// ```
    #[clippy::version = "pre 1.29.0"]
    pub MATCH_OVERLAPPING_ARM,
    style,
    "a `match` with overlapping arms"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for arm which matches all errors with `Err(_)`
    /// and take drastic actions like `panic!`.
    ///
    /// ### Why is this bad?
    /// It is generally a bad practice, similar to
    /// catching all exceptions in java with `catch(Exception)`
    ///
    /// ### Example
    /// ```rust
    /// let x: Result<i32, &str> = Ok(3);
    /// match x {
    ///     Ok(_) => println!("ok"),
    ///     Err(_) => panic!("err"),
    /// }
    /// ```
    #[clippy::version = "pre 1.29.0"]
    pub MATCH_WILD_ERR_ARM,
    pedantic,
    "a `match` with `Err(_)` arm and take drastic actions"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for match which is used to add a reference to an
    /// `Option` value.
    ///
    /// ### Why is this bad?
    /// Using `as_ref()` or `as_mut()` instead is shorter.
    ///
    /// ### Example
    /// ```rust
    /// let x: Option<()> = None;
    ///
    /// let r: Option<&()> = match x {
    ///     None => None,
    ///     Some(ref v) => Some(v),
    /// };
    /// ```
    ///
    /// Use instead:
    /// ```rust
    /// let x: Option<()> = None;
    ///
    /// let r: Option<&()> = x.as_ref();
    /// ```
    #[clippy::version = "pre 1.29.0"]
    pub MATCH_AS_REF,
    complexity,
    "a `match` on an Option value instead of using `as_ref()` or `as_mut`"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for wildcard enum matches using `_`.
    ///
    /// ### Why is this bad?
    /// New enum variants added by library updates can be missed.
    ///
    /// ### Known problems
    /// Suggested replacements may be incorrect if guards exhaustively cover some
    /// variants, and also may not use correct path to enum if it's not present in the current scope.
    ///
    /// ### Example
    /// ```rust
    /// # enum Foo { A(usize), B(usize) }
    /// # let x = Foo::B(1);
    /// match x {
    ///     Foo::A(_) => {},
    ///     _ => {},
    /// }
    /// ```
    ///
    /// Use instead:
    /// ```rust
    /// # enum Foo { A(usize), B(usize) }
    /// # let x = Foo::B(1);
    /// match x {
    ///     Foo::A(_) => {},
    ///     Foo::B(_) => {},
    /// }
    /// ```
    #[clippy::version = "1.34.0"]
    pub WILDCARD_ENUM_MATCH_ARM,
    restriction,
    "a wildcard enum match arm using `_`"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for wildcard enum matches for a single variant.
    ///
    /// ### Why is this bad?
    /// New enum variants added by library updates can be missed.
    ///
    /// ### Known problems
    /// Suggested replacements may not use correct path to enum
    /// if it's not present in the current scope.
    ///
    /// ### Example
    /// ```rust
    /// # enum Foo { A, B, C }
    /// # let x = Foo::B;
    /// match x {
    ///     Foo::A => {},
    ///     Foo::B => {},
    ///     _ => {},
    /// }
    /// ```
    ///
    /// Use instead:
    /// ```rust
    /// # enum Foo { A, B, C }
    /// # let x = Foo::B;
    /// match x {
    ///     Foo::A => {},
    ///     Foo::B => {},
    ///     Foo::C => {},
    /// }
    /// ```
    #[clippy::version = "1.45.0"]
    pub MATCH_WILDCARD_FOR_SINGLE_VARIANTS,
    pedantic,
    "a wildcard enum match for a single variant"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for wildcard pattern used with others patterns in same match arm.
    ///
    /// ### Why is this bad?
    /// Wildcard pattern already covers any other pattern as it will match anyway.
    /// It makes the code less readable, especially to spot wildcard pattern use in match arm.
    ///
    /// ### Example
    /// ```rust
    /// # let s = "foo";
    /// match s {
    ///     "a" => {},
    ///     "bar" | _ => {},
    /// }
    /// ```
    ///
    /// Use instead:
    /// ```rust
    /// # let s = "foo";
    /// match s {
    ///     "a" => {},
    ///     _ => {},
    /// }
    /// ```
    #[clippy::version = "1.42.0"]
    pub WILDCARD_IN_OR_PATTERNS,
    complexity,
    "a wildcard pattern used with others patterns in same match arm"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for matches being used to destructure a single-variant enum
    /// or tuple struct where a `let` will suffice.
    ///
    /// ### Why is this bad?
    /// Just readability – `let` doesn't nest, whereas a `match` does.
    ///
    /// ### Example
    /// ```rust
    /// enum Wrapper {
    ///     Data(i32),
    /// }
    ///
    /// let wrapper = Wrapper::Data(42);
    ///
    /// let data = match wrapper {
    ///     Wrapper::Data(i) => i,
    /// };
    /// ```
    ///
    /// The correct use would be:
    /// ```rust
    /// enum Wrapper {
    ///     Data(i32),
    /// }
    ///
    /// let wrapper = Wrapper::Data(42);
    /// let Wrapper::Data(data) = wrapper;
    /// ```
    #[clippy::version = "pre 1.29.0"]
    pub INFALLIBLE_DESTRUCTURING_MATCH,
    style,
    "a `match` statement with a single infallible arm instead of a `let`"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for useless match that binds to only one value.
    ///
    /// ### Why is this bad?
    /// Readability and needless complexity.
    ///
    /// ### Known problems
    ///  Suggested replacements may be incorrect when `match`
    /// is actually binding temporary value, bringing a 'dropped while borrowed' error.
    ///
    /// ### Example
    /// ```rust
    /// # let a = 1;
    /// # let b = 2;
    /// match (a, b) {
    ///     (c, d) => {
    ///         // useless match
    ///     }
    /// }
    /// ```
    ///
    /// Use instead:
    /// ```rust
    /// # let a = 1;
    /// # let b = 2;
    /// let (c, d) = (a, b);
    /// ```
    #[clippy::version = "1.43.0"]
    pub MATCH_SINGLE_BINDING,
    complexity,
    "a match with a single binding instead of using `let` statement"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for unnecessary '..' pattern binding on struct when all fields are explicitly matched.
    ///
    /// ### Why is this bad?
    /// Correctness and readability. It's like having a wildcard pattern after
    /// matching all enum variants explicitly.
    ///
    /// ### Example
    /// ```rust
    /// # struct A { a: i32 }
    /// let a = A { a: 5 };
    ///
    /// match a {
    ///     A { a: 5, .. } => {},
    ///     _ => {},
    /// }
    /// ```
    ///
    /// Use instead:
    /// ```rust
    /// # struct A { a: i32 }
    /// # let a = A { a: 5 };
    /// match a {
    ///     A { a: 5 } => {},
    ///     _ => {},
    /// }
    /// ```
    #[clippy::version = "1.43.0"]
    pub REST_PAT_IN_FULLY_BOUND_STRUCTS,
    restriction,
    "a match on a struct that binds all fields but still uses the wildcard pattern"
}

declare_clippy_lint! {
    /// ### What it does
    /// Lint for redundant pattern matching over `Result`, `Option`,
    /// `std::task::Poll` or `std::net::IpAddr`
    ///
    /// ### Why is this bad?
    /// It's more concise and clear to just use the proper
    /// utility function
    ///
    /// ### Known problems
    /// This will change the drop order for the matched type. Both `if let` and
    /// `while let` will drop the value at the end of the block, both `if` and `while` will drop the
    /// value before entering the block. For most types this change will not matter, but for a few
    /// types this will not be an acceptable change (e.g. locks). See the
    /// [reference](https://doc.rust-lang.org/reference/destructors.html#drop-scopes) for more about
    /// drop order.
    ///
    /// ### Example
    /// ```rust
    /// # use std::task::Poll;
    /// # use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    /// if let Ok(_) = Ok::<i32, i32>(42) {}
    /// if let Err(_) = Err::<i32, i32>(42) {}
    /// if let None = None::<()> {}
    /// if let Some(_) = Some(42) {}
    /// if let Poll::Pending = Poll::Pending::<()> {}
    /// if let Poll::Ready(_) = Poll::Ready(42) {}
    /// if let IpAddr::V4(_) = IpAddr::V4(Ipv4Addr::LOCALHOST) {}
    /// if let IpAddr::V6(_) = IpAddr::V6(Ipv6Addr::LOCALHOST) {}
    /// match Ok::<i32, i32>(42) {
    ///     Ok(_) => true,
    ///     Err(_) => false,
    /// };
    /// ```
    ///
    /// The more idiomatic use would be:
    ///
    /// ```rust
    /// # use std::task::Poll;
    /// # use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
    /// if Ok::<i32, i32>(42).is_ok() {}
    /// if Err::<i32, i32>(42).is_err() {}
    /// if None::<()>.is_none() {}
    /// if Some(42).is_some() {}
    /// if Poll::Pending::<()>.is_pending() {}
    /// if Poll::Ready(42).is_ready() {}
    /// if IpAddr::V4(Ipv4Addr::LOCALHOST).is_ipv4() {}
    /// if IpAddr::V6(Ipv6Addr::LOCALHOST).is_ipv6() {}
    /// Ok::<i32, i32>(42).is_ok();
    /// ```
    #[clippy::version = "1.31.0"]
    pub REDUNDANT_PATTERN_MATCHING,
    style,
    "use the proper utility function avoiding an `if let`"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for `match`  or `if let` expressions producing a
    /// `bool` that could be written using `matches!`
    ///
    /// ### Why is this bad?
    /// Readability and needless complexity.
    ///
    /// ### Known problems
    /// This lint falsely triggers, if there are arms with
    /// `cfg` attributes that remove an arm evaluating to `false`.
    ///
    /// ### Example
    /// ```rust
    /// let x = Some(5);
    ///
    /// let a = match x {
    ///     Some(0) => true,
    ///     _ => false,
    /// };
    ///
    /// let a = if let Some(0) = x {
    ///     true
    /// } else {
    ///     false
    /// };
    /// ```
    ///
    /// Use instead:
    /// ```rust
    /// let x = Some(5);
    /// let a = matches!(x, Some(0));
    /// ```
    #[clippy::version = "1.47.0"]
    pub MATCH_LIKE_MATCHES_MACRO,
    style,
    "a match that could be written with the matches! macro"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for `match` with identical arm bodies.
    ///
    /// ### Why is this bad?
    /// This is probably a copy & paste error. If arm bodies
    /// are the same on purpose, you can factor them
    /// [using `|`](https://doc.rust-lang.org/book/patterns.html#multiple-patterns).
    ///
    /// ### Known problems
    /// False positive possible with order dependent `match`
    /// (see issue
    /// [#860](https://github.com/rust-lang/rust-clippy/issues/860)).
    ///
    /// ### Example
    /// ```rust,ignore
    /// match foo {
    ///     Bar => bar(),
    ///     Quz => quz(),
    ///     Baz => bar(), // <= oops
    /// }
    /// ```
    ///
    /// This should probably be
    /// ```rust,ignore
    /// match foo {
    ///     Bar => bar(),
    ///     Quz => quz(),
    ///     Baz => baz(), // <= fixed
    /// }
    /// ```
    ///
    /// or if the original code was not a typo:
    /// ```rust,ignore
    /// match foo {
    ///     Bar | Baz => bar(), // <= shows the intent better
    ///     Quz => quz(),
    /// }
    /// ```
    #[clippy::version = "pre 1.29.0"]
    pub MATCH_SAME_ARMS,
    pedantic,
    "`match` with identical arm bodies"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for unnecessary `match` or match-like `if let` returns for `Option` and `Result`
    /// when function signatures are the same.
    ///
    /// ### Why is this bad?
    /// This `match` block does nothing and might not be what the coder intended.
    ///
    /// ### Example
    /// ```rust,ignore
    /// fn foo() -> Result<(), i32> {
    ///     match result {
    ///         Ok(val) => Ok(val),
    ///         Err(err) => Err(err),
    ///     }
    /// }
    ///
    /// fn bar() -> Option<i32> {
    ///     if let Some(val) = option {
    ///         Some(val)
    ///     } else {
    ///         None
    ///     }
    /// }
    /// ```
    ///
    /// Could be replaced as
    ///
    /// ```rust,ignore
    /// fn foo() -> Result<(), i32> {
    ///     result
    /// }
    ///
    /// fn bar() -> Option<i32> {
    ///     option
    /// }
    /// ```
    #[clippy::version = "1.61.0"]
    pub NEEDLESS_MATCH,
    complexity,
    "`match` or match-like `if let` that are unnecessary"
}

declare_clippy_lint! {
    /// ### What it does
    /// Finds nested `match` or `if let` expressions where the patterns may be "collapsed" together
    /// without adding any branches.
    ///
    /// Note that this lint is not intended to find _all_ cases where nested match patterns can be merged, but only
    /// cases where merging would most likely make the code more readable.
    ///
    /// ### Why is this bad?
    /// It is unnecessarily verbose and complex.
    ///
    /// ### Example
    /// ```rust
    /// fn func(opt: Option<Result<u64, String>>) {
    ///     let n = match opt {
    ///         Some(n) => match n {
    ///             Ok(n) => n,
    ///             _ => return,
    ///         }
    ///         None => return,
    ///     };
    /// }
    /// ```
    /// Use instead:
    /// ```rust
    /// fn func(opt: Option<Result<u64, String>>) {
    ///     let n = match opt {
    ///         Some(Ok(n)) => n,
    ///         _ => return,
    ///     };
    /// }
    /// ```
    #[clippy::version = "1.50.0"]
    pub COLLAPSIBLE_MATCH,
    style,
    "Nested `match` or `if let` expressions where the patterns may be \"collapsed\" together."
}

declare_clippy_lint! {
    /// ### What it does
    /// Finds patterns that reimplement `Option::unwrap_or` or `Result::unwrap_or`.
    ///
    /// ### Why is this bad?
    /// Concise code helps focusing on behavior instead of boilerplate.
    ///
    /// ### Example
    /// ```rust
    /// let foo: Option<i32> = None;
    /// match foo {
    ///     Some(v) => v,
    ///     None => 1,
    /// };
    /// ```
    ///
    /// Use instead:
    /// ```rust
    /// let foo: Option<i32> = None;
    /// foo.unwrap_or(1);
    /// ```
    #[clippy::version = "1.49.0"]
    pub MANUAL_UNWRAP_OR,
    complexity,
    "finds patterns that can be encoded more concisely with `Option::unwrap_or` or `Result::unwrap_or`"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for `match vec[idx]` or `match vec[n..m]`.
    ///
    /// ### Why is this bad?
    /// This can panic at runtime.
    ///
    /// ### Example
    /// ```rust, no_run
    /// let arr = vec![0, 1, 2, 3];
    /// let idx = 1;
    ///
    /// match arr[idx] {
    ///     0 => println!("{}", 0),
    ///     1 => println!("{}", 3),
    ///     _ => {},
    /// }
    /// ```
    ///
    /// Use instead:
    /// ```rust, no_run
    /// let arr = vec![0, 1, 2, 3];
    /// let idx = 1;
    ///
    /// match arr.get(idx) {
    ///     Some(0) => println!("{}", 0),
    ///     Some(1) => println!("{}", 3),
    ///     _ => {},
    /// }
    /// ```
    #[clippy::version = "1.45.0"]
    pub MATCH_ON_VEC_ITEMS,
    pedantic,
    "matching on vector elements can panic"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for `match` expressions modifying the case of a string with non-compliant arms
    ///
    /// ### Why is this bad?
    /// The arm is unreachable, which is likely a mistake
    ///
    /// ### Example
    /// ```rust
    /// # let text = "Foo";
    /// match &*text.to_ascii_lowercase() {
    ///     "foo" => {},
    ///     "Bar" => {},
    ///     _ => {},
    /// }
    /// ```
    /// Use instead:
    /// ```rust
    /// # let text = "Foo";
    /// match &*text.to_ascii_lowercase() {
    ///     "foo" => {},
    ///     "bar" => {},
    ///     _ => {},
    /// }
    /// ```
    #[clippy::version = "1.58.0"]
    pub MATCH_STR_CASE_MISMATCH,
    correctness,
    "creation of a case altering match expression with non-compliant arms"
}

declare_clippy_lint! {
    /// ### What it does
    /// Check for temporaries returned from function calls in a match scrutinee that have the
    /// `clippy::has_significant_drop` attribute.
    ///
    /// ### Why is this bad?
    /// The `clippy::has_significant_drop` attribute can be added to types whose Drop impls have
    /// an important side-effect, such as unlocking a mutex, making it important for users to be
    /// able to accurately understand their lifetimes. When a temporary is returned in a function
    /// call in a match scrutinee, its lifetime lasts until the end of the match block, which may
    /// be surprising.
    ///
    /// For `Mutex`es this can lead to a deadlock. This happens when the match scrutinee uses a
    /// function call that returns a `MutexGuard` and then tries to lock again in one of the match
    /// arms. In that case the `MutexGuard` in the scrutinee will not be dropped until the end of
    /// the match block and thus will not unlock.
    ///
    /// ### Example
    /// ```rust.ignore
    /// # use std::sync::Mutex;
    ///
    /// # struct State {}
    ///
    /// # impl State {
    /// #     fn foo(&self) -> bool {
    /// #         true
    /// #     }
    ///
    /// #     fn bar(&self) {}
    /// # }
    ///
    ///
    /// let mutex = Mutex::new(State {});
    ///
    /// match mutex.lock().unwrap().foo() {
    ///     true => {
    ///         mutex.lock().unwrap().bar(); // Deadlock!
    ///     }
    ///     false => {}
    /// };
    ///
    /// println!("All done!");
    ///
    /// ```
    /// Use instead:
    /// ```rust
    /// # use std::sync::Mutex;
    ///
    /// # struct State {}
    ///
    /// # impl State {
    /// #     fn foo(&self) -> bool {
    /// #         true
    /// #     }
    ///
    /// #     fn bar(&self) {}
    /// # }
    ///
    /// let mutex = Mutex::new(State {});
    ///
    /// let is_foo = mutex.lock().unwrap().foo();
    /// match is_foo {
    ///     true => {
    ///         mutex.lock().unwrap().bar();
    ///     }
    ///     false => {}
    /// };
    ///
    /// println!("All done!");
    /// ```
    #[clippy::version = "1.60.0"]
    pub SIGNIFICANT_DROP_IN_SCRUTINEE,
    suspicious,
    "warns when a temporary of a type with a drop with a significant side-effect might have a surprising lifetime"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for usages of `Err(x)?`.
    ///
    /// ### Why is this bad?
    /// The `?` operator is designed to allow calls that
    /// can fail to be easily chained. For example, `foo()?.bar()` or
    /// `foo(bar()?)`. Because `Err(x)?` can't be used that way (it will
    /// always return), it is more clear to write `return Err(x)`.
    ///
    /// ### Example
    /// ```rust
    /// fn foo(fail: bool) -> Result<i32, String> {
    ///     if fail {
    ///       Err("failed")?;
    ///     }
    ///     Ok(0)
    /// }
    /// ```
    /// Could be written:
    ///
    /// ```rust
    /// fn foo(fail: bool) -> Result<i32, String> {
    ///     if fail {
    ///       return Err("failed".into());
    ///     }
    ///     Ok(0)
    /// }
    /// ```
    #[clippy::version = "1.38.0"]
    pub TRY_ERR,
    restriction,
    "return errors explicitly rather than hiding them behind a `?`"
}

declare_clippy_lint! {
    /// ### What it does
    /// Checks for usages of `match` which could be implemented using `map`
    ///
    /// ### Why is this bad?
    /// Using the `map` method is clearer and more concise.
    ///
    /// ### Example
    /// ```rust
    /// match Some(0) {
    ///     Some(x) => Some(x + 1),
    ///     None => None,
    /// };
    /// ```
    /// Use instead:
    /// ```rust
    /// Some(0).map(|x| x + 1);
    /// ```
    #[clippy::version = "1.52.0"]
    pub MANUAL_MAP,
    style,
    "reimplementation of `map`"
}

#[derive(Default)]
pub struct Matches {
    msrv: Option<RustcVersion>,
    infallible_destructuring_match_linted: bool,
}

impl Matches {
    #[must_use]
    pub fn new(msrv: Option<RustcVersion>) -> Self {
        Self {
            msrv,
            ..Matches::default()
        }
    }
}

impl_lint_pass!(Matches => [
    SINGLE_MATCH,
    MATCH_REF_PATS,
    MATCH_BOOL,
    SINGLE_MATCH_ELSE,
    MATCH_OVERLAPPING_ARM,
    MATCH_WILD_ERR_ARM,
    MATCH_AS_REF,
    WILDCARD_ENUM_MATCH_ARM,
    MATCH_WILDCARD_FOR_SINGLE_VARIANTS,
    WILDCARD_IN_OR_PATTERNS,
    MATCH_SINGLE_BINDING,
    INFALLIBLE_DESTRUCTURING_MATCH,
    REST_PAT_IN_FULLY_BOUND_STRUCTS,
    REDUNDANT_PATTERN_MATCHING,
    MATCH_LIKE_MATCHES_MACRO,
    MATCH_SAME_ARMS,
    NEEDLESS_MATCH,
    COLLAPSIBLE_MATCH,
    MANUAL_UNWRAP_OR,
    MATCH_ON_VEC_ITEMS,
    MATCH_STR_CASE_MISMATCH,
    SIGNIFICANT_DROP_IN_SCRUTINEE,
    TRY_ERR,
    MANUAL_MAP,
]);

impl<'tcx> LateLintPass<'tcx> for Matches {
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'_>) {
        if in_external_macro(cx.sess(), expr.span) {
            return;
        }
        let from_expansion = expr.span.from_expansion();

        if let ExprKind::Match(ex, arms, source) = expr.kind {
            if source == MatchSource::Normal && !span_starts_with(cx, expr.span, "match") {
                return;
            }
            if matches!(source, MatchSource::Normal | MatchSource::ForLoopDesugar) {
                significant_drop_in_scrutinee::check(cx, expr, ex, source);
            }

            collapsible_match::check_match(cx, arms);
            if !from_expansion {
                // These don't depend on a relationship between multiple arms
                match_wild_err_arm::check(cx, ex, arms);
                wild_in_or_pats::check(cx, arms);
            }

            if source == MatchSource::TryDesugar {
                try_err::check(cx, expr, ex);
            }

            if !from_expansion && !contains_cfg_arm(cx, expr, ex, arms) {
                if source == MatchSource::Normal {
                    if !(meets_msrv(self.msrv, msrvs::MATCHES_MACRO)
                        && match_like_matches::check_match(cx, expr, ex, arms))
                    {
                        match_same_arms::check(cx, arms);
                    }

                    redundant_pattern_match::check_match(cx, expr, ex, arms);
                    single_match::check(cx, ex, arms, expr);
                    match_bool::check(cx, ex, arms, expr);
                    overlapping_arms::check(cx, ex, arms);
                    match_wild_enum::check(cx, ex, arms);
                    match_as_ref::check(cx, ex, arms, expr);
                    needless_match::check_match(cx, ex, arms, expr);
                    match_on_vec_items::check(cx, ex);
                    match_str_case_mismatch::check(cx, ex, arms);

                    if !in_constant(cx, expr.hir_id) {
                        manual_unwrap_or::check(cx, expr, ex, arms);
                        manual_map::check_match(cx, expr, ex, arms);
                    }

                    if self.infallible_destructuring_match_linted {
                        self.infallible_destructuring_match_linted = false;
                    } else {
                        match_single_binding::check(cx, ex, arms, expr);
                    }
                }
                match_ref_pats::check(cx, ex, arms.iter().map(|el| el.pat), expr);
            }
        } else if let Some(if_let) = higher::IfLet::hir(cx, expr) {
            collapsible_match::check_if_let(cx, if_let.let_pat, if_let.if_then, if_let.if_else);
            if !from_expansion {
                if let Some(else_expr) = if_let.if_else {
                    if meets_msrv(self.msrv, msrvs::MATCHES_MACRO) {
                        match_like_matches::check_if_let(
                            cx,
                            expr,
                            if_let.let_pat,
                            if_let.let_expr,
                            if_let.if_then,
                            else_expr,
                        );
                    }
                    if !in_constant(cx, expr.hir_id) {
                        manual_map::check_if_let(cx, expr, if_let.let_pat, if_let.let_expr, if_let.if_then, else_expr);
                    }
                }
                redundant_pattern_match::check_if_let(
                    cx,
                    expr,
                    if_let.let_pat,
                    if_let.let_expr,
                    if_let.if_else.is_some(),
                );
                needless_match::check_if_let(cx, expr, &if_let);
            }
        } else if !from_expansion {
            redundant_pattern_match::check(cx, expr);
        }
    }

    fn check_local(&mut self, cx: &LateContext<'tcx>, local: &'tcx Local<'_>) {
        self.infallible_destructuring_match_linted |= infallible_destructuring_match::check(cx, local);
    }

    fn check_pat(&mut self, cx: &LateContext<'tcx>, pat: &'tcx Pat<'_>) {
        rest_pat_in_fully_bound_struct::check(cx, pat);
    }

    extract_msrv_attr!(LateContext);
}

/// Checks if there are any arms with a `#[cfg(..)]` attribute.
fn contains_cfg_arm(cx: &LateContext<'_>, e: &Expr<'_>, scrutinee: &Expr<'_>, arms: &[Arm<'_>]) -> bool {
    let Some(scrutinee_span) = walk_span_to_context(scrutinee.span, SyntaxContext::root()) else {
        // Shouldn't happen, but treat this as though a `cfg` attribute were found
        return true;
    };

    let start = scrutinee_span.hi();
    let mut arm_spans = arms.iter().map(|arm| {
        let data = arm.span.data();
        (data.ctxt == SyntaxContext::root()).then(|| (data.lo, data.hi))
    });
    let end = e.span.hi();

    // Walk through all the non-code space before each match arm. The space trailing the final arm is
    // handled after the `try_fold` e.g.
    //
    // match foo {
    // _________^-                      everything between the scrutinee and arm1
    //|    arm1 => (),
    //|---^___________^                 everything before arm2
    //|    #[cfg(feature = "enabled")]
    //|    arm2 => some_code(),
    //|---^____________________^        everything before arm3
    //|    // some comment about arm3
    //|    arm3 => some_code(),
    //|---^____________________^        everything after arm3
    //|    #[cfg(feature = "disabled")]
    //|    arm4 = some_code(),
    //|};
    //|^
    let found = arm_spans.try_fold(start, |start, range| {
        let Some((end, next_start)) = range else {
            // Shouldn't happen as macros can't expand to match arms, but treat this as though a `cfg` attribute were
            // found.
            return Err(());
        };
        let span = SpanData {
            lo: start,
            hi: end,
            ctxt: SyntaxContext::root(),
            parent: None,
        }
        .span();
        (!span_contains_cfg(cx, span)).then(|| next_start).ok_or(())
    });
    match found {
        Ok(start) => {
            let span = SpanData {
                lo: start,
                hi: end,
                ctxt: SyntaxContext::root(),
                parent: None,
            }
            .span();
            span_contains_cfg(cx, span)
        },
        Err(()) => true,
    }
}

/// Checks if the given span contains a `#[cfg(..)]` attribute
fn span_contains_cfg(cx: &LateContext<'_>, s: Span) -> bool {
    let Some(snip) = snippet_opt(cx, s) else {
        // Assume true. This would require either an invalid span, or one which crosses file boundaries.
        return true;
    };
    let mut pos = 0usize;
    let mut iter = tokenize(&snip).map(|t| {
        let start = pos;
        pos += t.len;
        (t.kind, start..pos)
    });

    // Search for the token sequence [`#`, `[`, `cfg`]
    while iter.any(|(t, _)| matches!(t, TokenKind::Pound)) {
        let mut iter = iter.by_ref().skip_while(|(t, _)| {
            matches!(
                t,
                TokenKind::Whitespace | TokenKind::LineComment { .. } | TokenKind::BlockComment { .. }
            )
        });
        if matches!(iter.next(), Some((TokenKind::OpenBracket, _)))
            && matches!(iter.next(), Some((TokenKind::Ident, range)) if &snip[range.clone()] == "cfg")
        {
            return true;
        }
    }
    false
}
