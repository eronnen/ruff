use std::num::NonZeroUsize;
use std::ops::{Deref, Index};

use smallvec::SmallVec;

use ruff_formatter::write;
use ruff_python_ast::{
    Constant, Expr, ExprAttribute, ExprBinOp, ExprCompare, ExprConstant, ExprUnaryOp, UnaryOp,
};

use crate::comments::{leading_comments, trailing_comments, Comments, SourceComment};
use crate::expression::parentheses::{
    in_parentheses_only_group, in_parentheses_only_soft_line_break,
    in_parentheses_only_soft_line_break_or_space, is_expression_parenthesized,
    write_in_parentheses_only_group_end_tag, write_in_parentheses_only_group_start_tag,
};
use crate::expression::string::{AnyString, FormatString, StringLayout};
use crate::expression::OperatorPrecedence;
use crate::prelude::*;

#[derive(Copy, Clone, Debug)]
pub(super) enum BinaryLike<'a> {
    BinaryExpression(&'a ExprBinOp),
    CompareExpression(&'a ExprCompare),
}

impl<'a> BinaryLike<'a> {
    /// Flattens the hierarchical binary expression into a flat operand, operator, operand... sequence.
    ///
    /// See [`FlatBinaryExpressionSlice`] for an in depth explanation.
    fn flatten(self, comments: &'a Comments<'a>, source: &str) -> FlatBinaryExpression<'a> {
        fn recurse_compare<'a>(
            compare: &'a ExprCompare,
            leading_comments: &'a [SourceComment],
            trailing_comments: &'a [SourceComment],
            comments: &'a Comments,
            source: &str,
            parts: &mut SmallVec<[OperandOrOperator<'a>; 8]>,
        ) {
            parts.reserve(compare.comparators.len() * 2 + 1);

            rec(
                Operand::Left {
                    expression: &compare.left,
                    leading_comments,
                },
                comments,
                source,
                parts,
            );

            assert_eq!(
                compare.comparators.len(),
                compare.ops.len(),
                "Compare expression with an unbalanced number of comparators and operations."
            );

            if let Some((last_expression, middle_expressions)) = compare.comparators.split_last() {
                let (last_operator, middle_operators) = compare.ops.split_last().unwrap();

                for (operator, expression) in middle_operators.iter().zip(middle_expressions) {
                    parts.push(OperandOrOperator::Operator(Operator {
                        symbol: OperatorSymbol::Comparator(*operator),
                        trailing_comments: &[],
                    }));

                    rec(Operand::Middle { expression }, comments, source, parts);
                }

                parts.push(OperandOrOperator::Operator(Operator {
                    symbol: OperatorSymbol::Comparator(*last_operator),
                    trailing_comments: &[],
                }));

                rec(
                    Operand::Right {
                        expression: last_expression,
                        trailing_comments,
                    },
                    comments,
                    source,
                    parts,
                );
            }
        }

        fn recurse_binary<'a>(
            binary: &'a ExprBinOp,
            leading_comments: &'a [SourceComment],
            trailing_comments: &'a [SourceComment],
            comments: &'a Comments,
            source: &str,
            parts: &mut SmallVec<[OperandOrOperator<'a>; 8]>,
        ) {
            rec(
                Operand::Left {
                    leading_comments,
                    expression: &binary.left,
                },
                comments,
                source,
                parts,
            );

            parts.push(OperandOrOperator::Operator(Operator {
                symbol: OperatorSymbol::Binary(binary.op),
                trailing_comments: comments.dangling(binary),
            }));

            rec(
                Operand::Right {
                    expression: binary.right.as_ref(),
                    trailing_comments,
                },
                comments,
                source,
                parts,
            );
        }

        fn rec<'a>(
            operand: Operand<'a>,
            comments: &'a Comments,
            source: &str,
            parts: &mut SmallVec<[OperandOrOperator<'a>; 8]>,
        ) {
            let expression = operand.expression();
            match expression {
                Expr::BinOp(binary) if !is_expression_parenthesized(expression.into(), source) => {
                    let leading_comments = operand
                        .leading_binary_comments()
                        .unwrap_or_else(|| comments.leading(binary));

                    let trailing_comments = operand
                        .trailing_binary_comments()
                        .unwrap_or_else(|| comments.trailing(binary));

                    recurse_binary(
                        binary,
                        leading_comments,
                        trailing_comments,
                        comments,
                        source,
                        parts,
                    );
                }
                Expr::Compare(compare)
                    if !is_expression_parenthesized(expression.into(), source) =>
                {
                    let leading_comments = operand
                        .leading_binary_comments()
                        .unwrap_or_else(|| comments.leading(compare));

                    let trailing_comments = operand
                        .trailing_binary_comments()
                        .unwrap_or_else(|| comments.trailing(compare));

                    recurse_compare(
                        compare,
                        leading_comments,
                        trailing_comments,
                        comments,
                        source,
                        parts,
                    );
                }
                _ => {
                    parts.push(OperandOrOperator::Operand(operand));
                }
            }
        }

        let mut parts = SmallVec::new();
        match self {
            BinaryLike::BinaryExpression(binary) => {
                // Leading and trailing comments are handled by the binary's ``FormatNodeRule` implementation.
                recurse_binary(binary, &[], &[], comments, source, &mut parts);
            }
            BinaryLike::CompareExpression(compare) => {
                // Leading and trailing comments are handled by the compare's ``FormatNodeRule` implementation.
                recurse_compare(compare, &[], &[], comments, source, &mut parts);
            }
        }

        FlatBinaryExpression(parts)
    }
}

impl Format<PyFormatContext<'_>> for BinaryLike<'_> {
    fn fmt(&self, f: &mut Formatter<PyFormatContext<'_>>) -> FormatResult<()> {
        let comments = f.context().comments().clone();
        let flat_binary = self.flatten(&comments, f.context().source());

        let source = f.context().source();
        let mut string_operands = flat_binary
            .operands()
            .filter_map(|(index, operand)| {
                AnyString::from_expression(operand.expression())
                    .filter(|string| {
                        string.is_implicit_concatenated()
                            && !is_expression_parenthesized(string.into(), source)
                    })
                    .map(|string| (index, string, operand))
            })
            .peekable();

        // Split the binary expressions by implicit concatenated strings first by creating:
        // * One group that encloses the whole binary expression and ensures that all implicit concatenated strings
        //   break together or fit on the same line
        // * Group the left operand and left operator as well as the right operator and right operand
        //   to give them a lower precedence than the implicit concatenated string parts (the implicit strings should break first)
        if let Some((first_index, _, _)) = string_operands.peek() {
            // Group all strings in a single group so that they all break together or none of them.
            write_in_parentheses_only_group_start_tag(f);

            // Start the group for the left side coming before an implicit concatenated string if it isn't the first
            // ```python
            // a + "b" "c"
            // ^^^- start this group
            // ```
            if *first_index != OperandIndex::new(0) {
                write_in_parentheses_only_group_start_tag(f);
            }

            // The index of the last formatted operator
            let mut last_operator_index = None;

            loop {
                if let Some((index, string_constant, operand)) = string_operands.next() {
                    // An implicit concatenated string that isn't the first operand in a binary expression
                    // ```python
                    // a + "b" "c" + ddddddd + "e" "d"
                    //     ^^^^^^ this part or ^^^^^^^ this part
                    // ```
                    if let Some(left_operator_index) = index.left_operator() {
                        // Everything between the last implicit concatenated string and the left operator
                        // right before the implicit concatenated string:
                        // ```python
                        // a + b + "c" "d"
                        //       ^--- left_operator
                        // ^^^^^-- left
                        // ```
                        let left =
                            flat_binary.between_operators(last_operator_index, left_operator_index);
                        let left_operator = &flat_binary[left_operator_index];

                        if let Some(leading) = left.first_operand().leading_binary_comments() {
                            leading_comments(leading).fmt(f)?;
                        }

                        // Write the left, the left operator, and the space before the right side
                        write!(
                            f,
                            [
                                left,
                                left.last_operand()
                                    .trailing_binary_comments()
                                    .map(trailing_comments),
                                in_parentheses_only_soft_line_break_or_space(),
                                left_operator,
                            ]
                        )?;

                        // Finish the left-side group (the group was started before the loop or by the
                        // previous iteration)
                        write_in_parentheses_only_group_end_tag(f);

                        if operand.has_leading_comments(f.context().comments())
                            || left_operator.has_trailing_comments()
                        {
                            hard_line_break().fmt(f)?;
                        } else {
                            space().fmt(f)?;
                        }

                        write!(
                            f,
                            [
                                operand.leading_binary_comments().map(leading_comments),
                                leading_comments(comments.leading(&string_constant)),
                                FormatString::new(&string_constant).with_layout(
                                    StringLayout::ImplicitConcatenatedStringInBinaryLike,
                                ),
                                trailing_comments(comments.trailing(&string_constant)),
                                operand.trailing_binary_comments().map(trailing_comments),
                                line_suffix_boundary(),
                            ]
                        )?;
                    } else {
                        // Binary expression that starts with an implicit concatenated string:
                        // ```python
                        // "a" "b" + c
                        // ^^^^^^^-- format the first operand of a binary expression
                        // ```
                        write!(
                            f,
                            [
                                leading_comments(comments.leading(&string_constant)),
                                FormatString::new(&string_constant).with_layout(
                                    StringLayout::ImplicitConcatenatedStringInBinaryLike
                                ),
                                trailing_comments(comments.trailing(&string_constant)),
                            ]
                        )?;
                    }

                    // Write the right operator and start the group for the right side (if any)
                    // ```python
                    // a + "b" "c" + ddddddd + "e" "d"
                    //             ^^--- write this
                    //             ^^^^^^^^^^^-- start this group
                    // ```
                    let right_operator_index = index.right_operator();
                    if let Some(right_operator) = flat_binary.get_operator(index.right_operator()) {
                        write_in_parentheses_only_group_start_tag(f);
                        let right_operand = &flat_binary[right_operator_index.right_operand()];
                        let right_operand_has_leading_comments =
                            right_operand.has_leading_comments(f.context().comments());

                        // Keep the operator on the same line if the right side has leading comments (and thus, breaks)
                        if right_operand_has_leading_comments {
                            space().fmt(f)?;
                        } else {
                            in_parentheses_only_soft_line_break_or_space().fmt(f)?;
                        }

                        right_operator.fmt(f)?;

                        if right_operand_has_leading_comments
                            || right_operator.has_trailing_comments()
                        {
                            hard_line_break().fmt(f)?;
                        } else {
                            space().fmt(f)?;
                        }

                        last_operator_index = Some(right_operator_index);
                    } else {
                        break;
                    }
                } else {
                    if let Some(last_operator_index) = last_operator_index {
                        let end = flat_binary.after_operator(last_operator_index);

                        end.fmt(f)?;

                        write_in_parentheses_only_group_end_tag(f);
                    }

                    break;
                }
            }

            // Finish the group that wraps all implicit concatenated strings
            write_in_parentheses_only_group_end_tag(f);
        } else {
            in_parentheses_only_group(&&*flat_binary).fmt(f)?;
        }

        Ok(())
    }
}

const fn is_simple_power_expression(left: &Expr, right: &Expr) -> bool {
    is_simple_power_operand(left) && is_simple_power_operand(right)
}

/// Return `true` if an [`Expr`] adheres to [Black's definition](https://black.readthedocs.io/en/stable/the_black_code_style/current_style.html#line-breaks-binary-operators)
/// of a non-complex expression, in the context of a power operation.
const fn is_simple_power_operand(expr: &Expr) -> bool {
    match expr {
        Expr::UnaryOp(ExprUnaryOp {
            op: UnaryOp::Not, ..
        }) => false,
        Expr::Constant(ExprConstant {
            value: Constant::Complex { .. } | Constant::Float(_) | Constant::Int(_),
            ..
        }) => true,
        Expr::Name(_) => true,
        Expr::UnaryOp(ExprUnaryOp { operand, .. }) => is_simple_power_operand(operand),
        Expr::Attribute(ExprAttribute { value, .. }) => is_simple_power_operand(value),
        _ => false,
    }
}

/// Owned [`FlatBinaryExpressionSlice`]. Read the [`FlatBinaryExpressionSlice`] documentation for more details about the data structure.
#[derive(Debug)]
struct FlatBinaryExpression<'a>(SmallVec<[OperandOrOperator<'a>; 8]>);

impl<'a> Deref for FlatBinaryExpression<'a> {
    type Target = FlatBinaryExpressionSlice<'a>;

    fn deref(&self) -> &Self::Target {
        FlatBinaryExpressionSlice::from_slice(&self.0)
    }
}

/// Binary chain represented as a flat vector where operands are stored at even indices and operators
/// add odd indices.
///
/// ```python
/// a + 5 * 3 + 2
/// ```
///
/// Gets parsed as:
///
/// ```text
/// graph
/// +
/// ├──a
/// ├──*
/// │   ├──b
/// │   └──c
/// └──d
/// ```
///
/// The slice representation of the above is closer to what you have in source. It's a simple sequence of operands and operators,
/// entirely ignoring operator precedence (doesn't flatten parenthesized expressions):
///
/// ```text
/// -----------------------------
/// | a | + | 5 | * | 3 | + | 2 |
/// -----------------------------
/// ```
///
/// The advantage of a flat structure are:
/// * It becomes possible to adjust the operator / operand precedence. E.g splitting implicit concatenated strings before `+` operations.
/// * It allows arbitrary slicing of binary expressions for as long as a slice always starts and ends with an operand.
///
/// A slice is guaranteed to always start and end with an operand. The smallest valid slice is a slice containing a single operand.
/// Operands in multi-operand slices are separated by operators.
#[repr(transparent)]
struct FlatBinaryExpressionSlice<'a>([OperandOrOperator<'a>]);

impl<'a> FlatBinaryExpressionSlice<'a> {
    fn from_slice<'slice>(slice: &'slice [OperandOrOperator<'a>]) -> &'slice Self {
        debug_assert!(
            !slice.is_empty(),
            "Operand slice must contain at least one operand"
        );

        #[allow(unsafe_code)]
        unsafe {
            // SAFETY: `BinaryChainSlice` has the same layout as a slice because it uses `repr(transparent)`
            &*(slice as *const [OperandOrOperator<'a>] as *const FlatBinaryExpressionSlice<'a>)
        }
    }

    fn operators(&self) -> impl Iterator<Item = (OperatorIndex, &Operator<'a>)> {
        self.0.iter().enumerate().filter_map(|(index, part)| {
            if let OperandOrOperator::Operator(operator) = part {
                Some((OperatorIndex::new(index), operator))
            } else {
                None
            }
        })
    }

    fn operands(&self) -> impl Iterator<Item = (OperandIndex, &Operand<'a>)> {
        self.0.iter().enumerate().filter_map(|(index, part)| {
            if let OperandOrOperator::Operand(operand) = part {
                Some((OperandIndex::new(index), operand))
            } else {
                None
            }
        })
    }

    /// Creates a subslice that contains the operands coming after `last_operator` and up to, but not including the `end` operator.
    fn between_operators(&self, last_operator: Option<OperatorIndex>, end: OperatorIndex) -> &Self {
        let start = last_operator.map_or(0usize, |operator| operator.right_operand().0);
        Self::from_slice(&self.0[start..end.value()])
    }

    /// Creates a slice starting at the right operand of `index`.
    fn after_operator(&self, index: OperatorIndex) -> &Self {
        Self::from_slice(&self.0[index.right_operand().0..])
    }

    /// Returns the lowest precedence of any operator in this binary chain.
    fn lowest_precedence(&self) -> OperatorPrecedence {
        self.operators()
            .map(|(_, operator)| operator.precedence())
            .max()
            .unwrap_or(OperatorPrecedence::None)
    }

    /// Returns the first operand in the slice.
    fn first_operand(&self) -> &Operand<'a> {
        match self.0.first() {
            Some(OperandOrOperator::Operand(operand)) => operand,
            _ => unreachable!("Expected an operand"),
        }
    }

    /// Returns the last operand (the right most operand).
    fn last_operand(&self) -> &Operand<'a> {
        match self.0.last() {
            Some(OperandOrOperator::Operand(operand)) => operand,
            _ => unreachable!("Expected an operand"),
        }
    }

    /// Returns the operator at the given index or `None` if it is out of bounds.
    fn get_operator(&self, index: OperatorIndex) -> Option<&Operator<'a>> {
        self.0
            .get(index.value())
            .map(OperandOrOperator::unwrap_operator)
    }
}

/// Formats a binary chain slice by inserting soft line breaks before the lowest-precedence operators.
/// In other words: It splits the line before by the lowest precedence operators (and it either splits
/// all of them or none). For example, the lowest precedence operator for `a + b * c + d` is the `+` operator.
/// The expression either gets formatted as `a + b * c + d` if it fits on the line or as
/// ```python
/// a
/// + b * c
/// + d
/// ```
///
/// Notice how the formatting first splits by the lower precedence operator `+` but tries to keep the `*` operation
/// on a single line.
///
/// The formatting is recursive (with a depth of `O(operators)` where `operators` are operators with different precedences).
///
/// Comments before or after the first operand must be formatted by the caller because they shouldn't be part of the group
/// wrapping the whole binary chain. This is to avoid that `b * c` expands in the following example because of its trailing comemnt:
///
/// ```python
///
/// ( a
///     + b * c # comment
///     + d
/// )
/// ```
///
///
impl Format<PyFormatContext<'_>> for FlatBinaryExpressionSlice<'_> {
    fn fmt(&self, f: &mut Formatter<PyFormatContext>) -> FormatResult<()> {
        // Single operand slice
        if let [OperandOrOperator::Operand(operand)] = &self.0 {
            return operand.expression().format().fmt(f);
        }

        let mut last_operator: Option<OperatorIndex> = None;

        let lowest_precedence = self.lowest_precedence();

        for (index, operator_part) in self.operators() {
            if operator_part.precedence() == lowest_precedence {
                let left = self.between_operators(last_operator, index);
                let right = self.after_operator(index);

                let is_pow = operator_part.symbol.is_pow()
                    && is_simple_power_expression(
                        left.last_operand().expression(),
                        right.first_operand().expression(),
                    );

                if let Some(leading) = left.first_operand().leading_binary_comments() {
                    leading_comments(leading).fmt(f)?;
                }

                in_parentheses_only_group(&left).fmt(f)?;

                if let Some(trailing) = left.last_operand().trailing_binary_comments() {
                    trailing_comments(trailing).fmt(f)?;
                }

                if is_pow {
                    in_parentheses_only_soft_line_break().fmt(f)?;
                } else {
                    in_parentheses_only_soft_line_break_or_space().fmt(f)?;
                }

                operator_part.fmt(f)?;

                // Format the operator on its own line if the right side has any leading comments.
                if right
                    .first_operand()
                    .has_leading_comments(f.context().comments())
                    || operator_part.has_trailing_comments()
                {
                    hard_line_break().fmt(f)?;
                } else if !is_pow {
                    space().fmt(f)?;
                }

                last_operator = Some(index);
            }
        }

        // Format the last right side
        // SAFETY: It is guaranteed that the slice contains at least a operand, operator, operand sequence or:
        //  * the slice contains only a single operand in which case the function exits early above.
        //  * the slice is empty, which isn't a valid slice
        //  * the slice violates the operand, operator, operand constraint, in which case the error already happened earlier.
        let right = self.after_operator(last_operator.unwrap());

        if let Some(leading) = right.first_operand().leading_binary_comments() {
            leading_comments(leading).fmt(f)?;
        }

        in_parentheses_only_group(&right).fmt(f)
    }
}

/// Either an [`Operand`] or [`Operator`]
#[derive(Debug)]
enum OperandOrOperator<'a> {
    Operand(Operand<'a>),
    Operator(Operator<'a>),
}

impl<'a> OperandOrOperator<'a> {
    fn unwrap_operand(&self) -> &Operand<'a> {
        match self {
            OperandOrOperator::Operand(operand) => operand,
            OperandOrOperator::Operator(operator) => {
                panic!("Expected operand but found operator {operator:?}.")
            }
        }
    }

    fn unwrap_operator(&self) -> &Operator<'a> {
        match self {
            OperandOrOperator::Operator(operator) => operator,
            OperandOrOperator::Operand(operand) => {
                panic!("Expected operator but found operand {operand:?}.")
            }
        }
    }
}

#[derive(Debug)]
enum Operand<'a> {
    /// Operand that used to be on the left side of a binary operation.
    ///
    /// For example `a` in the following code
    ///
    /// ```python
    /// a + b + c
    /// ```
    Left {
        expression: &'a Expr,
        /// Leading comments of the outer most binary expression that starts at this node.
        leading_comments: &'a [SourceComment],
    },
    /// Operand that is neither at the start nor the end of a binary like expression.
    /// Only applies to compare expression.
    ///
    /// `b` and `c` are *middle* operands whereas `a` is a left and `d` a right operand.
    ///
    /// ```python
    /// a > b > c > d
    /// ```
    ///
    /// Middle have no leading or trailing comments from the enclosing binary like expression.
    Middle { expression: &'a Expr },

    /// Operand that is on the right side of a binary operation.
    ///
    /// For example `b` and `c` are right sides of the binary expressions.
    ///
    /// ```python
    /// a + b + c
    /// ```
    Right {
        expression: &'a Expr,
        /// Trailing comments of the outer most binary expression that ends at this operand.
        trailing_comments: &'a [SourceComment],
    },
}

impl<'a> Operand<'a> {
    fn expression(&self) -> &'a Expr {
        match self {
            Operand::Left { expression, .. } => expression,
            Operand::Right { expression, .. } => expression,
            Operand::Middle { expression } => expression,
        }
    }

    fn has_leading_comments(&self, comments: &Comments) -> bool {
        match self {
            Operand::Left {
                leading_comments, ..
            } => !leading_comments.is_empty(),
            Operand::Middle { expression } | Operand::Right { expression, .. } => {
                comments.has_leading(*expression)
            }
        }
    }

    /// Comments of the outer-most enclosing binary expression.
    fn leading_binary_comments(&self) -> Option<&'a [SourceComment]> {
        match self {
            Operand::Left {
                leading_comments, ..
            } => Some(leading_comments),
            Operand::Middle { .. } | Operand::Right { .. } => None,
        }
    }

    fn trailing_binary_comments(&self) -> Option<&'a [SourceComment]> {
        match self {
            Operand::Middle { .. } | Operand::Left { .. } => None,
            Operand::Right {
                trailing_comments, ..
            } => Some(trailing_comments),
        }
    }
}

#[derive(Debug)]
struct Operator<'a> {
    symbol: OperatorSymbol,
    trailing_comments: &'a [SourceComment],
}

impl Operator<'_> {
    fn precedence(&self) -> OperatorPrecedence {
        self.symbol.precedence()
    }

    fn has_trailing_comments(&self) -> bool {
        !self.trailing_comments.is_empty()
    }
}

impl Format<PyFormatContext<'_>> for Operator<'_> {
    fn fmt(&self, f: &mut Formatter<PyFormatContext<'_>>) -> FormatResult<()> {
        write!(f, [self.symbol, trailing_comments(self.trailing_comments)])
    }
}

#[derive(Copy, Clone, Debug)]
enum OperatorSymbol {
    Binary(ruff_python_ast::Operator),
    Comparator(ruff_python_ast::CmpOp),
}

impl OperatorSymbol {
    const fn is_pow(self) -> bool {
        matches!(self, OperatorSymbol::Binary(ruff_python_ast::Operator::Pow))
    }

    fn precedence(self) -> OperatorPrecedence {
        match self {
            OperatorSymbol::Binary(operator) => OperatorPrecedence::from(operator),
            OperatorSymbol::Comparator(_) => OperatorPrecedence::Comparator,
        }
    }
}

impl Format<PyFormatContext<'_>> for OperatorSymbol {
    fn fmt(&self, f: &mut Formatter<PyFormatContext<'_>>) -> FormatResult<()> {
        match self {
            OperatorSymbol::Binary(operator) => operator.format().fmt(f),
            OperatorSymbol::Comparator(operator) => operator.format().fmt(f),
        }
    }
}

/// Index of an Operand in the [`FlatBinaryExpressionSlice`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Ord, PartialOrd)]
struct OperandIndex(usize);

impl OperandIndex {
    fn new(index: usize) -> Self {
        debug_assert_eq!(index % 2, 0, "Operand indices must be even positions");

        Self(index)
    }

    /// Returns the index of the operator directly left to this operand. Returns [`None`] if
    /// this is the first operand in the call chain.
    fn left_operator(self) -> Option<OperatorIndex> {
        if self.0 == 0 {
            None
        } else {
            Some(OperatorIndex::new(self.0 - 1))
        }
    }

    /// Returns the index of the operand's right operator. The method always returns an index
    /// even if the operand has no right operator. Use [`BinaryCallChain::get_operator`] to test if
    /// the operand has a right operator.
    fn right_operator(self) -> OperatorIndex {
        OperatorIndex::new(self.0 + 1)
    }
}

/// Index of an Operator in the [`FlatBinaryExpressionSlice`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Ord, PartialOrd)]
struct OperatorIndex(NonZeroUsize);

impl OperatorIndex {
    fn new(index: usize) -> Self {
        assert_eq!(index % 2, 1, "Operator indices must be odd positions");

        // SAFETY A value with a module 0 is guaranteed to never equal 0
        #[allow(unsafe_code)]
        Self(unsafe { NonZeroUsize::new_unchecked(index) })
    }

    const fn value(self) -> usize {
        self.0.get()
    }

    fn right_operand(self) -> OperandIndex {
        OperandIndex::new(self.value() + 1)
    }
}

impl<'a> Index<OperatorIndex> for FlatBinaryExpressionSlice<'a> {
    type Output = Operator<'a>;

    fn index(&self, index: OperatorIndex) -> &Self::Output {
        self.0[index.value()].unwrap_operator()
    }
}

impl<'a> Index<OperandIndex> for FlatBinaryExpressionSlice<'a> {
    type Output = Operand<'a>;

    fn index(&self, index: OperandIndex) -> &Self::Output {
        self.0[index.0].unwrap_operand()
    }
}

mod size_assertion {
    use super::{FlatBinaryExpressionSlice, OperandOrOperator, OperatorIndex};

    static_assertions::assert_eq_size!(Option<OperatorIndex>, OperatorIndex);

    static_assertions::assert_eq_size!(&FlatBinaryExpressionSlice, &[OperandOrOperator]);
    static_assertions::assert_eq_align!(&FlatBinaryExpressionSlice, &[OperandOrOperator]);
}
