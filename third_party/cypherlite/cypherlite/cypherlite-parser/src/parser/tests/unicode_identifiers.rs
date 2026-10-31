// ADR-0182 / LABS-188: the CYPHER 25 unquoted identifier rule is a BMP-only
// Unicode 13.0.0 `General_Category` + `Pattern_Syntax` predicate:
//
//   start = {Lu, Ll, Lt, Lm, Lo, Nl, Pc} \ Pattern_Syntax
//   part  = {Lu, Ll, Lt, Lm, Lo, Nl, Pc, Mn, Mc, Nd, Sc} \ Pattern_Syntax
//
// The rows below are the `start`, `part`, `label`, `reltype`, `property`,
// `mapkey`, `variable`, `alias` and `parameter` rows of the Neo4j 2026.09
// `CYPHER 25` probe table (`probe_table.json`, LABS-188): the recorded
// `cypher25` outcome must be the parse outcome here. Invisible code points
// are written `\u{...}`. One row is an ASCII keyword-alias control rather
// than an identifier-character behavior; it is asserted separately below.

use super::*;

#[rustfmt::skip]
const PROBE_ROWS: &[(&str, &str, Option<char>, bool)] = &[
    ("RETURN 1 AS Ax", "start", Some('A'), true),
    ("RETURN 1 AS xA", "part", Some('A'), true),
    ("RETURN 1 AS Éx", "start", Some('É'), true),
    ("RETURN 1 AS xÉ", "part", Some('É'), true),
    ("RETURN 1 AS Жx", "start", Some('Ж'), true),
    ("RETURN 1 AS xЖ", "part", Some('Ж'), true),
    ("RETURN 1 AS ax", "start", Some('a'), true),
    ("RETURN 1 AS xa", "part", Some('a'), true),
    ("RETURN 1 AS éx", "start", Some('é'), true),
    ("RETURN 1 AS xé", "part", Some('é'), true),
    ("RETURN 1 AS ſx", "start", Some('ſ'), true),
    ("RETURN 1 AS xſ", "part", Some('ſ'), true),
    ("RETURN 1 AS ǅx", "start", Some('ǅ'), true),
    ("RETURN 1 AS xǅ", "part", Some('ǅ'), true),
    ("RETURN 1 AS ǈx", "start", Some('ǈ'), true),
    ("RETURN 1 AS xǈ", "part", Some('ǈ'), true),
    ("RETURN 1 AS ǋx", "start", Some('ǋ'), true),
    ("RETURN 1 AS xǋ", "part", Some('ǋ'), true),
    ("RETURN 1 AS ʰx", "start", Some('ʰ'), true),
    ("RETURN 1 AS xʰ", "part", Some('ʰ'), true),
    ("RETURN 1 AS ʷx", "start", Some('ʷ'), true),
    ("RETURN 1 AS xʷ", "part", Some('ʷ'), true),
    ("RETURN 1 AS ʹx", "start", Some('ʹ'), true),
    ("RETURN 1 AS xʹ", "part", Some('ʹ'), true),
    ("RETURN 1 AS 一x", "start", Some('一'), true),
    ("RETURN 1 AS x一", "part", Some('一'), true),
    ("RETURN 1 AS あx", "start", Some('あ'), true),
    ("RETURN 1 AS xあ", "part", Some('あ'), true),
    ("RETURN 1 AS กx", "start", Some('ก'), true),
    ("RETURN 1 AS xก", "part", Some('ก'), true),
    ("RETURN 1 AS \u{0301}x", "start", Some('\u{0301}'), false),
    ("RETURN 1 AS x\u{0301}", "part", Some('\u{0301}'), true),
    ("RETURN 1 AS \u{0300}x", "start", Some('\u{0300}'), false),
    ("RETURN 1 AS x\u{0300}", "part", Some('\u{0300}'), true),
    ("RETURN 1 AS \u{0651}x", "start", Some('\u{0651}'), false),
    ("RETURN 1 AS x\u{0651}", "part", Some('\u{0651}'), true),
    ("RETURN 1 AS \u{0903}x", "start", Some('\u{0903}'), false),
    ("RETURN 1 AS x\u{0903}", "part", Some('\u{0903}'), true),
    ("RETURN 1 AS \u{0982}x", "start", Some('\u{0982}'), false),
    ("RETURN 1 AS x\u{0982}", "part", Some('\u{0982}'), true),
    ("RETURN 1 AS \u{0F3E}x", "start", Some('\u{0F3E}'), false),
    ("RETURN 1 AS x\u{0F3E}", "part", Some('\u{0F3E}'), true),
    ("RETURN 1 AS \u{0488}x", "start", Some('\u{0488}'), false),
    ("RETURN 1 AS x\u{0488}", "part", Some('\u{0488}'), false),
    ("RETURN 1 AS \u{0489}x", "start", Some('\u{0489}'), false),
    ("RETURN 1 AS x\u{0489}", "part", Some('\u{0489}'), false),
    ("RETURN 1 AS \u{20DD}x", "start", Some('\u{20DD}'), false),
    ("RETURN 1 AS x\u{20DD}", "part", Some('\u{20DD}'), false),
    ("RETURN 1 AS 0x", "start", Some('0'), false),
    ("RETURN 1 AS x0", "part", Some('0'), true),
    ("RETURN 1 AS ٠x", "start", Some('٠'), false),
    ("RETURN 1 AS x٠", "part", Some('٠'), true),
    ("RETURN 1 AS １x", "start", Some('１'), false),
    ("RETURN 1 AS x１", "part", Some('１'), true),
    ("RETURN 1 AS Ⅰx", "start", Some('Ⅰ'), true),
    ("RETURN 1 AS xⅠ", "part", Some('Ⅰ'), true),
    ("RETURN 1 AS Ⅻx", "start", Some('Ⅻ'), true),
    ("RETURN 1 AS xⅫ", "part", Some('Ⅻ'), true),
    ("RETURN 1 AS ᛮx", "start", Some('ᛮ'), true),
    ("RETURN 1 AS xᛮ", "part", Some('ᛮ'), true),
    ("RETURN 1 AS ²x", "start", Some('²'), false),
    ("RETURN 1 AS x²", "part", Some('²'), false),
    ("RETURN 1 AS ½x", "start", Some('½'), false),
    ("RETURN 1 AS x½", "part", Some('½'), false),
    ("RETURN 1 AS ⁰x", "start", Some('⁰'), false),
    ("RETURN 1 AS x⁰", "part", Some('⁰'), false),
    ("RETURN 1 AS _x", "start", Some('_'), true),
    ("RETURN 1 AS x_", "part", Some('_'), true),
    ("RETURN 1 AS ‿x", "start", Some('‿'), true),
    ("RETURN 1 AS x‿", "part", Some('‿'), true),
    ("RETURN 1 AS ⁔x", "start", Some('⁔'), true),
    ("RETURN 1 AS x⁔", "part", Some('⁔'), true),
    ("RETURN 1 AS -x", "start", Some('-'), false),
    ("RETURN 1 AS x-", "part", Some('-'), false),
    ("RETURN 1 AS ‐x", "start", Some('‐'), false),
    ("RETURN 1 AS x‐", "part", Some('‐'), false),
    ("RETURN 1 AS –x", "start", Some('–'), false),
    ("RETURN 1 AS x–", "part", Some('–'), false),
    ("RETURN 1 AS (x", "start", Some('('), false),
    ("RETURN 1 AS x(", "part", Some('('), false),
    ("RETURN 1 AS [x", "start", Some('['), false),
    ("RETURN 1 AS x[", "part", Some('['), false),
    ("RETURN 1 AS {x", "start", Some('{'), false),
    ("RETURN 1 AS x{", "part", Some('{'), false),
    ("RETURN 1 AS )x", "start", Some(')'), false),
    ("RETURN 1 AS x)", "part", Some(')'), false),
    ("RETURN 1 AS ]x", "start", Some(']'), false),
    ("RETURN 1 AS x]", "part", Some(']'), false),
    ("RETURN 1 AS }x", "start", Some('}'), false),
    ("RETURN 1 AS x}", "part", Some('}'), false),
    ("RETURN 1 AS «x", "start", Some('«'), false),
    ("RETURN 1 AS x«", "part", Some('«'), false),
    ("RETURN 1 AS ‘x", "start", Some('‘'), false),
    ("RETURN 1 AS x‘", "part", Some('‘'), false),
    ("RETURN 1 AS “x", "start", Some('“'), false),
    ("RETURN 1 AS x“", "part", Some('“'), false),
    ("RETURN 1 AS »x", "start", Some('»'), false),
    ("RETURN 1 AS x»", "part", Some('»'), false),
    ("RETURN 1 AS ’x", "start", Some('’'), false),
    ("RETURN 1 AS x’", "part", Some('’'), false),
    ("RETURN 1 AS ”x", "start", Some('”'), false),
    ("RETURN 1 AS x”", "part", Some('”'), false),
    ("RETURN 1 AS !x", "start", Some('!'), false),
    ("RETURN 1 AS x!", "part", Some('!'), false),
    ("RETURN 1 AS ?x", "start", Some('?'), false),
    ("RETURN 1 AS x?", "part", Some('?'), false),
    ("RETURN 1 AS %x", "start", Some('%'), false),
    ("RETURN 1 AS x%", "part", Some('%'), false),
    ("RETURN 1 AS +x", "start", Some('+'), false),
    ("RETURN 1 AS x+", "part", Some('+'), false),
    ("RETURN 1 AS =x", "start", Some('='), false),
    ("RETURN 1 AS x=", "part", Some('='), false),
    ("RETURN 1 AS ×x", "start", Some('×'), false),
    ("RETURN 1 AS x×", "part", Some('×'), false),
    ("RETURN 1 AS ¢x", "start", Some('¢'), false),
    ("RETURN 1 AS x¢", "part", Some('¢'), false),
    ("RETURN 1 AS £x", "start", Some('£'), false),
    ("RETURN 1 AS x£", "part", Some('£'), false),
    ("RETURN 1 AS €x", "start", Some('€'), false),
    ("RETURN 1 AS x€", "part", Some('€'), true),
    ("RETURN 1 AS ¥x", "start", Some('¥'), false),
    ("RETURN 1 AS x¥", "part", Some('¥'), false),
    ("RETURN 1 AS ^x", "start", Some('^'), false),
    ("RETURN 1 AS x^", "part", Some('^'), false),
    ("RETURN 1 AS `x", "start", Some('`'), false),
    ("RETURN 1 AS x`", "part", Some('`'), false),
    ("RETURN 1 AS ¨x", "start", Some('¨'), false),
    ("RETURN 1 AS x¨", "part", Some('¨'), false),
    ("RETURN 1 AS ©x", "start", Some('©'), false),
    ("RETURN 1 AS x©", "part", Some('©'), false),
    ("RETURN 1 AS ®x", "start", Some('®'), false),
    ("RETURN 1 AS x®", "part", Some('®'), false),
    ("RETURN 1 AS ☃x", "start", Some('☃'), false),
    ("RETURN 1 AS x☃", "part", Some('☃'), false),
    ("RETURN 1 AS  x", "start", Some(' '), true),
    ("RETURN 1 AS x ", "part", Some(' '), true),
    ("RETURN 1 AS  x", "start", Some(' '), true),
    ("RETURN 1 AS x ", "part", Some(' '), true),
    ("RETURN 1 AS  x", "start", Some(' '), true),
    ("RETURN 1 AS x ", "part", Some(' '), true),
    ("RETURN 1 AS 　x", "start", Some('　'), true),
    ("RETURN 1 AS x　", "part", Some('　'), true),
    ("RETURN 1 AS \u{061C}x", "start", Some('\u{061C}'), false),
    ("RETURN 1 AS x\u{061C}", "part", Some('\u{061C}'), false),
    ("RETURN 1 AS \u{200E}x", "start", Some('\u{200E}'), false),
    ("RETURN 1 AS x\u{200E}", "part", Some('\u{200E}'), false),
    ("RETURN 1 AS \u{200F}x", "start", Some('\u{200F}'), false),
    ("RETURN 1 AS x\u{200F}", "part", Some('\u{200F}'), false),
    ("RETURN 1 AS \u{2060}x", "start", Some('\u{2060}'), false),
    ("RETURN 1 AS x\u{2060}", "part", Some('\u{2060}'), false),
    ("RETURN 1 AS \u{E000}x", "start", Some('\u{E000}'), false),
    ("RETURN 1 AS x\u{E000}", "part", Some('\u{E000}'), false),
    ("RETURN 1 AS \u{E001}x", "start", Some('\u{E001}'), false),
    ("RETURN 1 AS x\u{E001}", "part", Some('\u{E001}'), false),
    ("RETURN 1 AS \u{F8FF}x", "start", Some('\u{F8FF}'), false),
    ("RETURN 1 AS x\u{F8FF}", "part", Some('\u{F8FF}'), false),
    ("RETURN 1 AS $x", "start", Some('$'), false),
    ("RETURN 1 AS x$", "part", Some('$'), false),
    ("RETURN 1 AS \u{00AD}x", "start", Some('\u{00AD}'), false),
    ("RETURN 1 AS x\u{00AD}", "part", Some('\u{00AD}'), false),
    ("RETURN 1 AS ·x", "start", Some('·'), false),
    ("RETURN 1 AS x·", "part", Some('·'), false),
    ("RETURN 1 AS ·x", "start", Some('·'), false),
    ("RETURN 1 AS x·", "part", Some('·'), false),
    ("RETURN 1 AS ፩x", "start", Some('፩'), false),
    ("RETURN 1 AS x፩", "part", Some('፩'), false),
    ("RETURN 1 AS ᧚x", "start", Some('᧚'), false),
    ("RETURN 1 AS x᧚", "part", Some('᧚'), false),
    ("RETURN 1 AS \u{200B}x", "start", Some('\u{200B}'), false),
    ("RETURN 1 AS x\u{200B}", "part", Some('\u{200B}'), false),
    ("RETURN 1 AS \u{200C}x", "start", Some('\u{200C}'), false),
    ("RETURN 1 AS x\u{200C}", "part", Some('\u{200C}'), false),
    ("RETURN 1 AS \u{200D}x", "start", Some('\u{200D}'), false),
    ("RETURN 1 AS x\u{200D}", "part", Some('\u{200D}'), false),
    ("RETURN 1 AS ℘x", "start", Some('℘'), false),
    ("RETURN 1 AS x℘", "part", Some('℘'), false),
    ("RETURN 1 AS ℮x", "start", Some('℮'), false),
    ("RETURN 1 AS x℮", "part", Some('℮'), false),
    ("RETURN 1 AS ⸯx", "start", Some('ⸯ'), false),
    ("RETURN 1 AS xⸯ", "part", Some('ⸯ'), false),
    ("RETURN 1 AS ゛x", "start", Some('゛'), false),
    ("RETURN 1 AS x゛", "part", Some('゛'), false),
    ("RETURN 1 AS ゜x", "start", Some('゜'), false),
    ("RETURN 1 AS x゜", "part", Some('゜'), false),
    ("RETURN 1 AS \u{FEFF}x", "start", Some('\u{FEFF}'), false),
    ("RETURN 1 AS x\u{FEFF}", "part", Some('\u{FEFF}'), false),
    ("RETURN 1 AS \u{0085}x", "start", Some('\u{0085}'), true),
    ("RETURN 1 AS x\u{0085}", "part", Some('\u{0085}'), true),
    ("RETURN 1 AS \u{180E}x", "start", Some('\u{180E}'), false),
    ("RETURN 1 AS x\u{180E}", "part", Some('\u{180E}'), false),
    ("RETURN 1 AS 𐐀x", "start", Some('𐐀'), false),
    ("RETURN 1 AS x𐐀", "part", Some('𐐀'), false),
    ("RETURN 1 AS 𝐀x", "start", Some('𝐀'), false),
    ("RETURN 1 AS x𝐀", "part", Some('𝐀'), false),
    ("RETURN 1 AS 𐌰x", "start", Some('𐌰'), false),
    ("RETURN 1 AS x𐌰", "part", Some('𐌰'), false),
    ("RETURN 1 AS ﬁx", "start", Some('ﬁ'), true),
    ("RETURN 1 AS xﬁ", "part", Some('ﬁ'), true),
    ("RETURN 1 AS x y", "part", Some(' '), false),
    ("RETURN 1 AS x y", "part", Some(' '), false),
    ("RETURN 1 AS x y", "part", Some(' '), false),
    ("RETURN 1 AS x　y", "part", Some('　'), false),
    ("RETURN 1 AS x\u{061C}y", "part", Some('\u{061C}'), false),
    ("RETURN 1 AS x\u{200E}y", "part", Some('\u{200E}'), false),
    ("RETURN 1 AS x\u{200F}y", "part", Some('\u{200F}'), false),
    ("RETURN 1 AS x\u{2060}y", "part", Some('\u{2060}'), false),
    ("RETURN 1 AS x\u{00AD}y", "part", Some('\u{00AD}'), false),
    ("RETURN 1 AS x\u{200B}y", "part", Some('\u{200B}'), false),
    ("RETURN 1 AS x\u{200C}y", "part", Some('\u{200C}'), false),
    ("RETURN 1 AS x\u{200D}y", "part", Some('\u{200D}'), false),
    ("RETURN 1 AS x\u{FEFF}y", "part", Some('\u{FEFF}'), false),
    ("RETURN 1 AS x\u{180E}y", "part", Some('\u{180E}'), false),
    ("MATCH (n:名前) RETURN 1 AS x", "label", None, true),
    ("MATCH ()-[r:名前]->() RETURN 1 AS x", "reltype", None, true),
    ("WITH {a: 1} AS m RETURN m.名前 AS x", "property", None, true),
    ("RETURN {名前: 1} AS m", "mapkey", None, true),
    ("WITH 1 AS 名前 RETURN 名前 AS x", "variable", None, true),
    ("RETURN 1 AS 名前", "alias", None, true),
    ("RETURN $名前 AS v", "parameter", None, true),
    ("MATCH (n:café) RETURN 1 AS x", "label", None, true),
    ("MATCH ()-[r:café]->() RETURN 1 AS x", "reltype", None, true),
    ("WITH {a: 1} AS m RETURN m.café AS x", "property", None, true),
    ("RETURN {café: 1} AS m", "mapkey", None, true),
    ("WITH 1 AS café RETURN café AS x", "variable", None, true),
    ("RETURN 1 AS café", "alias", None, true),
    ("RETURN $café AS v", "parameter", None, true),
    ("RETURN 1 AS ſKIP", "alias", Some('ſ'), true),
    ("WITH 1 AS ſKIP RETURN ſKIP AS x", "variable", Some('ſ'), true),
    ("RETURN 1 AS SKIP", "alias", None, true),
    ("RETURN 1 AS ͺx", "start", Some('ͺ'), true),
    ("RETURN 1 AS xͺ", "part", Some('ͺ'), true),
    ("RETURN 1 AS ำx", "start", Some('ำ'), true),
    ("RETURN 1 AS xำ", "part", Some('ำ'), true),
    ("RETURN 1 AS ﾞx", "start", Some('ﾞ'), true),
    ("RETURN 1 AS xﾞ", "part", Some('ﾞ'), true),
    ("RETURN 1 AS ﱞx", "start", Some('ﱞ'), true),
    ("RETURN 1 AS xﱞ", "part", Some('ﱞ'), true),
    ("RETURN 1 AS ﹰx", "start", Some('ﹰ'), true),
    ("RETURN 1 AS xﹰ", "part", Some('ﹰ'), true),
    ("RETURN 1 AS \u{1885}x", "start", Some('\u{1885}'), false),
    ("RETURN 1 AS x\u{1885}", "part", Some('\u{1885}'), true),
    ("RETURN 1 AS \u{1886}x", "start", Some('\u{1886}'), false),
    ("RETURN 1 AS x\u{1886}", "part", Some('\u{1886}'), true),
    ("RETURN 1 AS ・x", "start", Some('・'), false),
    ("RETURN 1 AS x・", "part", Some('・'), false),
    ("RETURN 1 AS ･x", "start", Some('･'), false),
    ("RETURN 1 AS x･", "part", Some('･'), false),
    ("RETURN 1 AS ーx", "start", Some('ー'), true),
    ("RETURN 1 AS xー", "part", Some('ー'), true),
    ("RETURN 1 AS 々x", "start", Some('々'), true),
    ("RETURN 1 AS x々", "part", Some('々'), true),
    ("RETURN 1 AS ࡰx", "start", Some('ࡰ'), false),
    ("RETURN 1 AS xࡰ", "part", Some('ࡰ'), false),
    ("RETURN 1 AS ࣈx", "start", Some('ࣈ'), false),
    ("RETURN 1 AS xࣈ", "part", Some('ࣈ'), false),
    ("RETURN 1 AS Ⱟx", "start", Some('Ⱟ'), false),
    ("RETURN 1 AS xⰯ", "part", Some('Ⱟ'), false),
    ("RETURN 1 AS 鿽x", "start", Some('鿽'), false),
    ("RETURN 1 AS x鿽", "part", Some('鿽'), false),
    ("RETURN 1 AS ⃀x", "start", Some('⃀'), false),
    ("RETURN 1 AS x⃀", "part", Some('⃀'), false),
    ("RETURN 1 AS \u{0898}x", "start", Some('\u{0898}'), false),
    ("RETURN 1 AS x\u{0898}", "part", Some('\u{0898}'), false),
    ("RETURN 1 AS \u{1715}x", "start", Some('\u{1715}'), false),
    ("RETURN 1 AS x\u{1715}", "part", Some('\u{1715}'), false),
    ("RETURN 1 AS \u{180F}x", "start", Some('\u{180F}'), false),
    ("RETURN 1 AS x\u{180F}", "part", Some('\u{180F}'), false),
    ("RETURN 1 AS ࢾx", "start", Some('ࢾ'), true),
    ("RETURN 1 AS xࢾ", "part", Some('ࢾ'), true),
    ("RETURN 1 AS 鿼x", "start", Some('鿼'), true),
    ("RETURN 1 AS x鿼", "part", Some('鿼'), true),
    ("RETURN 1 AS Ꞻx", "start", Some('Ꞻ'), true),
    ("RETURN 1 AS xꞺ", "part", Some('Ꞻ'), true),
    ("RETURN 1 AS ֏x", "start", Some('֏'), false),
    ("RETURN 1 AS x֏", "part", Some('֏'), true),
    ("RETURN 1 AS ߾x", "start", Some('߾'), false),
    ("RETURN 1 AS x߾", "part", Some('߾'), true),
    ("RETURN 1 AS ߿x", "start", Some('߿'), false),
    ("RETURN 1 AS x߿", "part", Some('߿'), true),
    ("RETURN 1 AS ꠸x", "start", Some('꠸'), false),
    ("RETURN 1 AS x꠸", "part", Some('꠸'), true),
    ("RETURN 1 AS ¤x", "start", Some('¤'), false),
    ("RETURN 1 AS x¤", "part", Some('¤'), false),
    ("RETURN 1 AS ＄x", "start", Some('＄'), false),
    ("RETURN 1 AS x＄", "part", Some('＄'), true),
    ("RETURN 1 AS a$b", "part", Some('$'), false),
    ("RETURN 1 AS x$y", "part", Some('$'), false),
];

/// The one probe row that is an ASCII keyword-alias control rather than an
/// identifier-character behavior: `RETURN 1 AS SKIP`. The lexer still
/// lexes `SKIP` as the `SKIP` keyword, which is exactly what ADR-0182
/// requires; since ADR-0191 every keyword is a symbolic name after `AS`,
/// so the alias is the keyword spelling.
const KEYWORD_ALIAS_CONTROL: &str = "RETURN 1 AS SKIP";

#[test]
fn test_probe_rows_match_cypher_25() {
    for &(query, position, _, accepted) in PROBE_ROWS {
        let parsed = parse(query);
        if accepted {
            assert!(
                parsed.is_ok(),
                "[{position}] {query:?} must parse under CYPHER 25: {:?}",
                parsed.err()
            );
        } else {
            assert!(
                parsed.is_err(),
                "[{position}] {query:?} must fail the parse under CYPHER 25"
            );
        }
    }
    // Every probe row is accounted for: all 287 Unicode-identifier rows run
    // in this loop; the ASCII keyword-alias control also has its own
    // spelling assertion below.
    assert_eq!(PROBE_ROWS.len(), 287);
}

#[test]
fn test_keyword_alias_control_row_parses_with_skip_as_an_alias() {
    // `RETURN 1 AS SKIP` (probe position `alias`, codepoint null) is the
    // ASCII control for the U+017F `ſKIP` lookalike rows. ADR-0191 makes
    // every keyword a symbolic name after `AS`, so the SKIP keyword token
    // is kept as the alias.
    let stmt = parse(KEYWORD_ALIAS_CONTROL).expect("SKIP is an alias after AS");
    match &stmt.clauses[0] {
        Clause::Return(r) => {
            assert_eq!(r.items[0].alias.as_deref(), Some("SKIP"));
            assert_eq!(r.items[0].expr, Expr::Literal(LiteralValue::Integer(1)));
        }
        other => panic!("expected RETURN, got {other:?}"),
    }
}

#[test]
fn test_non_start_code_points_fail_with_invalid_unicode_character() {
    // Every probe row that rejects a non-ASCII, non-whitespace code point
    // must reject it as ONE `InvalidUnicodeCharacter` (the part-2 LABS-188
    // shape): a start position that is not an identifier start, or a part
    // position that is not an identifier part, including supplementary-plane
    // characters, `Pattern_Syntax` members, `Other_ID_Start`/`Continue`
    // code points and Unicode 14+ additions the 13.0.0 tables do not know.
    // Whitespace code points are skipped: there the error is raised at the
    // following token, not at the whitespace itself.
    let mut checked = 0;
    for &(query, position, codepoint, accepted) in PROBE_ROWS {
        let Some(c) = codepoint else { continue };
        if accepted || c.is_ascii() || c.is_whitespace() {
            continue;
        }
        checked += 1;
        let err = parse(query).expect_err(query);
        assert_eq!(
            err.detail,
            Some("InvalidUnicodeCharacter"),
            "[{position}] {query:?} ({c:?}) must report InvalidUnicodeCharacter, got: {err}"
        );
        assert!(
            message_carries(&err.message, c),
            "[{position}] {query:?}: the error must carry the decoded {c:?}: {}",
            err.message
        );
    }
    // 142 probe rows exercise a rejected non-ASCII, non-whitespace code
    // point; the count keeps the loop from silently filtering them all out.
    assert_eq!(checked, 142);
}

/// True when `message` carries `c` either literally or as the `\u{...}`
/// debug escape Rust's `{:?}` formatting uses for non-printable characters
/// (the parser echoes the offending token with `{:?}`).
fn message_carries(message: &str, c: char) -> bool {
    message.contains(c) || message.contains(&format!("\\u{{{:x}}}", c as u32))
}

#[test]
fn test_number_glued_to_identifier_characters_is_invalid_number_literal() {
    // `answers.number_glued`: the number reader greedily consumes following
    // identifier characters into the numeric token, so `RETURN 1é` (like the
    // ASCII control `1x`) is one `InvalidNumberLiteral`, not a number
    // followed by a stray character.
    for query in [
        "RETURN 1é",
        "RETURN 1é AS x",
        "RETURN 1é + 1 AS x",
        "RETURN 1x AS x",
    ] {
        let err = parse(query).unwrap_err();
        assert_eq!(err.detail, Some("InvalidNumberLiteral"), "{query:?}: {err}");
        assert!(
            err.message.contains("invalid number literal"),
            "{query:?}: {}",
            err.message
        );
    }
    // The malformed run is consumed whole: the message carries the glued
    // non-ASCII identifier characters, not just the digits.
    let err = parse("RETURN 1é AS x").unwrap_err();
    assert!(err.message.contains("1é"), "{}", err.message);
    // The ASCII `1e` control of `answers.number_glued` is a malformed float
    // on its own pre-existing path (`invalid float literal: 1e`, no detail);
    // it must still be rejected, like Neo4j's 42I07.
    assert!(parse("RETURN 1e AS x").is_err());
}

#[test]
fn test_parameter_names_follow_the_identifier_rule() {
    // `answers.parameters`: parameter names follow the same unquoted
    // identifier rule as variables and labels; `$` is the sigil and is
    // never part of an identifier.
    for (query, name) in [("RETURN $名前 AS v", "名前"), ("RETURN $café AS v", "café")] {
        let stmt = parse(query).unwrap_or_else(|e| panic!("{query:?} must parse: {e}"));
        match &stmt.clauses[0] {
            Clause::Return(r) => match &r.items[0].expr {
                Expr::Parameter(n) => assert_eq!(n, name, "{query:?}"),
                other => panic!("{query:?}: expected a parameter, got {other:?}"),
            },
            other => panic!("{query:?}: expected RETURN, got {other:?}"),
        }
    }
    for query in [
        "RETURN 1 AS $x",
        "RETURN 1 AS x$",
        "RETURN 1 AS a$b",
        "RETURN 1 AS x$y",
    ] {
        assert!(parse(query).is_err(), "{query:?} must fail: $ is the sigil");
    }
}

#[test]
fn test_keyword_lookalike_is_an_identifier() {
    // `answers.keyword_lookalike`: U+017F is a letter, so `ſKIP` lexes as
    // one identifier and is never case-folded to the `SKIP` keyword.
    assert!(parse("RETURN 1 AS ſKIP").is_ok());
    assert!(parse("WITH 1 AS ſKIP RETURN ſKIP AS x").is_ok());
    assert!(parse("RETURN 1 ſKIP 0").is_err());
    // The ASCII control: `SKIP` stays the keyword (`RETURN 1 SKIP 0` parses
    // as a SKIP clause).
    assert!(parse("RETURN 1 SKIP 0").is_ok());
}

#[test]
fn test_error_column_stays_byte_based_and_caret_sits_under_the_char() {
    // ADR-0182: lexer columns stay byte-based (`error.rs` is unchanged and
    // converts the column to a char index only for the caret). `é` is two
    // bytes, so `§` is at byte column 19 while its char index is 17.
    let query = "RETURN 1 AS café §";
    let err = parse(query).unwrap_err();
    assert_eq!(err.detail, Some("InvalidUnicodeCharacter"), "{err}");
    assert_eq!(err.line, 1, "{err}");
    assert_eq!(err.col, query.find('§').unwrap() + 1, "byte column: {err}");
    assert_eq!(err.col, 19, "{err}");
    let rendered = err.to_string();
    let mut lines = rendered.lines();
    assert!(lines.next().unwrap().starts_with("parse error:"));
    let excerpt = lines.next().expect("excerpt").strip_prefix("  ").unwrap();
    let caret = lines.next().expect("caret").strip_prefix("  ").unwrap();
    assert_eq!(excerpt, query);
    assert_eq!(
        caret.chars().position(|c| c == '^'),
        query.chars().position(|c| c == '§'),
        "the caret must sit under §:\n{rendered}"
    );
}

#[test]
fn test_invalid_unicode_hyphen_detail_and_position() {
    // Mathematical3 [1]: `RETURN 42 — 41` (U+2014) must fail at the em dash
    // (byte column 11) with the TCK `InvalidUnicodeCharacter` detail.
    let err = parse("RETURN 42 \u{2014} 41").unwrap_err();
    assert_eq!(
        err.detail,
        Some("InvalidUnicodeCharacter"),
        "got {:?}: {err}",
        err.detail
    );
    assert_eq!((err.line, err.col), (1, 11), "{err}");
}
