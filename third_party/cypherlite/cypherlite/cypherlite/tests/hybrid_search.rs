//! CIP-278 hybrid search: the `COMBINE` source specification behind the
//! ADR-0371 `hybrid-search` preview gate. The movie corpus and every query
//! are the CIP's own appendix dataset; the WRRF-fused expectations are
//! doc-derived (the CIP's first implementation is not in a release, so
//! there is no live instance to verify against) and pinned to this
//! engine's deterministic rank orders.

mod common;

use cypherlite::Value;

/// The CIP-278 preview gate is process-level (ADR-0371: `Database::open`
/// resolves `CYPHERLITE_PREVIEW` once), and an integration-test crate is
/// its own process -- set it before any store opens and keep every test
/// in this crate behind the gate.
fn gated_connection() -> cypherlite::Connection {
    // Fail loudly (the same contract as the preview fixture in
    // `cypherlite/src/tests/preview.rs`) rather than run half-gated.
    common::set_preview_once("hybrid-search");
    common::connection()
}

/// The CIP-278 appendix "Graph setup queries": eight movies with plot
/// text and a 3-dimensional stand-in embedding, plus the vector and
/// fulltext indexes over them.
fn movie_store() -> cypherlite::Connection {
    let mut c = gated_connection();
    c.query(
        "CREATE FULLTEXT INDEX moviePlotFulltext IF NOT EXISTS \
         FOR (movie:Movie) ON EACH [movie.plot]",
    )
    .unwrap();
    c.query(
        "CREATE VECTOR INDEX moviePlotEmbeddings IF NOT EXISTS \
         FOR (movie:Movie) ON movie.embedding \
         OPTIONS {indexConfig: {`vector.dimensions`: 3, \
         `vector.similarity_function`: 'cosine'}}",
    )
    .unwrap();
    c.query(
        "CREATE (m1:Movie {id: 'movie-little-mermaid', title: 'The Little Mermaid', \
           plot: 'A mermaid princess trades her voice to a sea witch so she can leave her \
                  underwater kingdom and live on land with a human prince.', \
           embedding: [0.95, 0.70, 0.40]}), \
         (m2:Movie {id: 'movie-moana', title: 'Moana', \
           plot: 'A wayfinding princess sails beyond the reef to restore the heart of an \
                  island goddess and save her ocean village.', \
           embedding: [0.97, 0.60, 0.20]}), \
         (m3:Movie {id: 'movie-frozen', title: 'Frozen', \
           plot: 'A princess who can freeze anything she touches accidentally traps her \
                  kingdom in eternal winter.', \
           embedding: [0.05, 0.90, 0.85]}), \
         (m4:Movie {id: 'movie-aladdin', title: 'Aladdin', \
           plot: 'A street thief finds a magic lamp and a genie who can grant him three \
                  wishes to win a princess heart.', \
           embedding: [0.00, 0.50, 0.90]}), \
         (m5:Movie {id: 'movie-lion-king', title: 'The Lion King', \
           plot: 'A lion cub flees his kingdom after a tragic betrayal and must reclaim \
                  his throne from his uncle.', \
           embedding: [0.00, 0.90, 0.10]}), \
         (m6:Movie {id: 'movie-beauty-and-the-beast', title: 'Beauty and the Beast', \
           plot: 'A young woman is held captive in an enchanted castle by a prince who \
                  was cursed to live as a beast.', \
           embedding: [0.00, 0.60, 0.80]}), \
         (m7:Movie {id: 'movie-tangled', title: 'Tangled', \
           plot: 'A princess with magically long hair is locked in a tower by a witch \
                  pretending to be her mother.', \
           embedding: [0.00, 0.80, 0.90]}), \
         (m8:Movie {id: 'movie-finding-nemo', title: 'Finding Nemo', \
           plot: 'A clownfish father crosses the open ocean to find his son who was \
                  captured by a diver near the reef.', \
           embedding: [0.98, 0.00, 0.00]})",
    )
    .unwrap();
    c
}

/// The fused result titles, in the engine's fused order (score descending,
/// entity id ascending on ties).
fn titles(result: &cypherlite::QueryResult) -> Vec<String> {
    result
        .rows
        .iter()
        .map(|row| match &row[0] {
            Value::String(title) => title.to_string(),
            other => panic!("expected a title string, got {other:?}"),
        })
        .collect()
}

/// The CIP-278 "Proposal" showcase query: the weighted vector + fulltext
/// hybrid over the appendix corpus, weights 0.7/0.3, constant 60, fused
/// LIMIT 5. The CIP's own result table (The Little Mermaid, Moana, Frozen,
/// Tangled, The Lion King) is the doc-derived expectation.
#[test]
fn combine_fuses_vector_and_fulltext_with_wrrf() {
    let mut c = movie_store();
    let result = c
        .query(
            "MATCH (node) \
             SEARCH node IN ( \
               COMBINE ( \
                 VECTOR INDEX moviePlotEmbeddings \
                   FOR [0.95, 0.70, 0.40] LIMIT 10 WEIGHT 0.7, \
                 FULLTEXT INDEX moviePlotFulltext \
                   FOR 'mermaid princess witch underwater kingdom' LIMIT 10 WEIGHT 0.3 \
               ) USING WRRF WITH CONSTANT 60 LIMIT 5 \
             ) SCORE AS score \
             RETURN node.title AS title ORDER BY score DESC",
        )
        .unwrap();
    assert_eq!(
        titles(&result),
        vec![
            "The Little Mermaid",
            "Moana",
            "Frozen",
            "Tangled",
            "The Lion King",
        ]
    );
}

/// Example R2&4.1: omitted WEIGHTs default to 1 and an omitted WITH
/// CONSTANT defaults to 60 (unweighted RRF), with no fused LIMIT. The
/// CIP's own table orders Tangled before Frozen and puts The Lion King
/// fifth (its Lucene fulltext tie-break); this engine ties the fused
/// scores of Moana/Frozen exactly and orders ties by entity id ascending
/// (the CIP removed its documented tie-breaker -- implementation-dependent,
/// CLG-102), so Frozen (earlier id) precedes Tangled, and Aladdin's
/// both-source contributions edge The Lion King's by a hair for fifth.
#[test]
fn combine_defaults_weight_one_and_constant_sixty() {
    let mut c = movie_store();
    let result = c
        .query(
            "MATCH (node) \
             SEARCH node IN ( \
               COMBINE ( \
                 VECTOR INDEX moviePlotEmbeddings \
                   FOR [0.95, 0.70, 0.40] LIMIT 10, \
                 FULLTEXT INDEX moviePlotFulltext \
                   FOR 'mermaid princess witch underwater kingdom' LIMIT 10 \
               ) USING WRRF \
             ) SCORE AS score \
             RETURN node.title AS title ORDER BY score DESC LIMIT 5",
        )
        .unwrap();
    assert_eq!(
        titles(&result),
        vec![
            "The Little Mermaid",
            "Moana",
            "Frozen",
            "Tangled",
            "Aladdin",
        ]
    );
}

/// The fused `SCORE AS` column is FLOAT NOT NULL (the semantics section):
/// two sources both ranking an entity first contribute exactly
/// `0.7/(60+1) + 0.3/(60+1) = 1/61`.
#[test]
fn combine_score_is_the_wrrf_sum() {
    let mut c = movie_store();
    let result = c
        .query(
            "MATCH (node) \
             SEARCH node IN ( \
               COMBINE ( \
                 VECTOR INDEX moviePlotEmbeddings \
                   FOR [0.95, 0.70, 0.40] LIMIT 10 WEIGHT 0.7, \
                 FULLTEXT INDEX moviePlotFulltext \
                   FOR 'mermaid princess witch underwater kingdom' LIMIT 10 WEIGHT 0.3 \
               ) USING WRRF WITH CONSTANT 60 \
             ) SCORE AS score \
             RETURN score ORDER BY score DESC LIMIT 1",
        )
        .unwrap();
    let Value::Float(top) = result.rows[0][0] else {
        panic!("expected a FLOAT score, got {:?}", result.rows[0][0]);
    };
    assert!((top - 1.0 / 61.0).abs() < 1e-12, "top score {top}");
}

/// A source an entity is missing from contributes nothing from that
/// source (the WRRF "missing from a source" rule): Finding Nemo is the
/// vector search's rank 3 (0-based 2) but matches no fulltext term, so
/// its fused score is exactly the vector contribution `1/(60+2+1)`.
#[test]
fn combine_missing_source_contributes_zero() {
    let mut c = movie_store();
    let result = c
        .query(
            "MATCH (node) \
             SEARCH node IN ( \
               COMBINE ( \
                 VECTOR INDEX moviePlotEmbeddings \
                   FOR [0.95, 0.70, 0.40] LIMIT 10, \
                 FULLTEXT INDEX moviePlotFulltext \
                   FOR 'mermaid princess witch underwater kingdom' LIMIT 10 \
               ) USING WRRF \
             ) SCORE AS score \
             RETURN node.title AS title, score",
        )
        .unwrap();
    let nemo = result
        .rows
        .iter()
        .find(|row| matches!(&row[0], Value::String(t) if t.as_ref() == "Finding Nemo"))
        .expect("Finding Nemo rides the vector source alone");
    let Value::Float(score) = nemo[1] else {
        panic!("expected a FLOAT score, got {:?}", nemo[1]);
    };
    assert!((score - 1.0 / 63.0).abs() < 1e-12, "Nemo score {score}");
}

/// Example P3: a nested COMBINE source fuses its own sources first, then
/// ranks into the outer fusion under its own weight. The CIP's own table
/// heads with Frozen then Tangled (its Lucene fulltext tie-break); this
/// engine agrees on the head (the middle order differs on the same
/// implementation-dependent tie-break, CLG-102).
#[test]
fn combine_nests_a_combine_source() {
    let mut c = movie_store();
    c.query(
        "CREATE VECTOR INDEX movieArchetypeEmbeddings IF NOT EXISTS \
         FOR (movie:Movie) ON movie.archetypeEmbedding \
         OPTIONS {indexConfig: {`vector.dimensions`: 3, \
         `vector.similarity_function`: 'cosine'}}",
    )
    .unwrap();
    c.query(
        "MATCH (m:Movie) WHERE m.id IN ['movie-little-mermaid', 'movie-moana', \
         'movie-frozen', 'movie-aladdin', 'movie-lion-king', \
         'movie-beauty-and-the-beast', 'movie-tangled', 'movie-finding-nemo'] \
         SET m.archetypeEmbedding = CASE m.id \
           WHEN 'movie-little-mermaid' THEN [0.90, 0.10, 0.20] \
           WHEN 'movie-moana' THEN [0.10, 0.10, 0.95] \
           WHEN 'movie-frozen' THEN [0.30, 0.85, 0.10] \
           WHEN 'movie-aladdin' THEN [0.85, 0.20, 0.10] \
           WHEN 'movie-lion-king' THEN [0.05, 0.95, 0.30] \
           WHEN 'movie-beauty-and-the-beast' THEN [0.90, 0.15, 0.05] \
           WHEN 'movie-tangled' THEN [0.40, 0.60, 0.10] \
           WHEN 'movie-finding-nemo' THEN [0.05, 0.05, 0.95] END",
    )
    .unwrap();
    let result = c
        .query(
            "MATCH (node) \
             SEARCH node IN ( \
               COMBINE ( \
                 VECTOR INDEX moviePlotEmbeddings \
                   FOR [0.00, 0.85, 0.85] LIMIT 10 WEIGHT 1.0, \
                 COMBINE ( \
                   VECTOR INDEX movieArchetypeEmbeddings \
                     FOR [0.35, 0.65, 0.05] LIMIT 10 WEIGHT 1.3, \
                   FULLTEXT INDEX moviePlotFulltext \
                     FOR 'mermaid princess witch underwater kingdom' LIMIT 10 WEIGHT 0.7 \
                 ) USING WRRF WEIGHT 0.7 \
               ) USING WRRF \
             ) SCORE AS score \
             RETURN node.title AS title ORDER BY score DESC LIMIT 5",
        )
        .unwrap();
    let got = titles(&result);
    assert_eq!(got.len(), 5, "{got:?}");
    assert_eq!(got[0], "Frozen", "{got:?}");
    assert_eq!(got[1], "Tangled", "{got:?}");
}

/// Rule 1 (Example R1.1): a WEIGHT that is not INTEGER NOT NULL or FLOAT
/// NOT NULL fails with the invalid-type error.
#[test]
fn combine_rejects_a_non_numeric_weight() {
    let mut c = movie_store();
    let error = common::err_text(c.query(
        "MATCH (node) \
             SEARCH node IN ( \
               COMBINE ( \
                 VECTOR INDEX moviePlotEmbeddings \
                   FOR [0.95, 0.70, 0.40] LIMIT 10 WEIGHT 'not a number', \
                 FULLTEXT INDEX moviePlotFulltext \
                   FOR 'mermaid princess witch underwater kingdom' LIMIT 10 WEIGHT 0.3 \
               ) USING WRRF WITH CONSTANT 60 \
             ) SCORE AS score \
             RETURN node, score",
    ));
    assert!(
        error.contains(
            "Expected the value to be of type INTEGER NOT NULL or FLOAT NOT NULL, \
             but was of type STRING NOT NULL (SEARCH WEIGHT)"
        ),
        "{error}"
    );
}

/// Rule 3 (Example R3.1): a WITH CONSTANT that is not INTEGER NOT NULL or
/// FLOAT NOT NULL fails the same way.
#[test]
fn combine_rejects_a_non_numeric_constant() {
    let mut c = movie_store();
    let error = common::err_text(c.query(
        "MATCH (node) \
             SEARCH node IN ( \
               COMBINE ( \
                 VECTOR INDEX moviePlotEmbeddings \
                   FOR [0.95, 0.70, 0.40] LIMIT 10 WEIGHT 0.7, \
                 FULLTEXT INDEX moviePlotFulltext \
                   FOR 'mermaid princess witch underwater kingdom' LIMIT 10 WEIGHT 0.3 \
               ) USING WRRF WITH CONSTANT 'not a number' \
             ) SCORE AS score \
             RETURN node, score",
    ));
    assert!(
        error.contains(
            "Expected the value to be of type INTEGER NOT NULL or FLOAT NOT NULL, \
             but was of type STRING NOT NULL (SEARCH CONSTANT)"
        ),
        "{error}"
    );
}

/// Rule 6 (Example R6.1): a WEIGHT expression may not reference the search
/// clause's own binding variable.
#[test]
fn combine_rejects_a_self_referencing_weight() {
    let mut c = movie_store();
    let error = common::err_text(c.query(
        "MATCH (node) \
             SEARCH node IN ( \
               COMBINE ( \
                 VECTOR INDEX moviePlotEmbeddings \
                   FOR [0.95, 0.70, 0.40] LIMIT 10 WEIGHT 0.3 * node.weight, \
                 FULLTEXT INDEX moviePlotFulltext \
                   FOR 'mermaid princess witch underwater kingdom' LIMIT 10 WEIGHT 0.3 \
               ) USING WRRF WITH CONSTANT 60 \
             ) SCORE AS score \
             RETURN node, score",
    ));
    assert!(
        error.contains(
            "SelfReferencingSearch: the WEIGHT or CONSTANT expression \
             may not depend on the search clause binding variable 'node'"
        ),
        "{error}"
    );
}

/// Rule 5 (Example R5.1): a single weighted source fails the parse with
/// the missing-source error.
#[test]
fn combine_requires_two_sources() {
    let mut c = movie_store();
    let error = common::err_text(c.query(
        "MATCH (node) \
             SEARCH node IN ( \
               COMBINE ( \
                 VECTOR INDEX moviePlotEmbeddings \
                   FOR [0.95, 0.70, 0.40] LIMIT 10 WEIGHT 0.3 \
               ) USING WRRF WITH CONSTANT 60 \
             ) SCORE AS score \
             RETURN node, score",
    ));
    assert!(
        error.contains("The COMBINE clause requires two or more sources provided"),
        "{error}"
    );
}
