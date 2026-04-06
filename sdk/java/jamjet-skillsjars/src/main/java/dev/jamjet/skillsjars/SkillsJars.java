package dev.jamjet.skillsjars;

/**
 * JamJet SkillsJars — reference skill implementations for AI agent workflows.
 *
 * <p>This module provides pre-built skill JARs (data-enrichment, doc-summarization,
 * sentiment-analysis, etc.) that can be loaded by the JamJet runtime. Skills are
 * defined as resource files in this JAR and discovered at runtime via the
 * {@code META-INF/skills/} directory.
 *
 * <h2>Available Skills</h2>
 * <ul>
 *   <li>data-enrichment — enrich structured data with external sources</li>
 *   <li>doc-summarization — summarize documents using LLM</li>
 *   <li>sentiment-analysis — classify text sentiment</li>
 *   <li>code-review — automated code review with configurable rules</li>
 *   <li>rag-retrieval — retrieval-augmented generation patterns</li>
 *   <li>approval-workflow — human-in-the-loop approval patterns</li>
 * </ul>
 *
 * <h2>Usage</h2>
 * <p>Add this module to your classpath. Skills are auto-discovered by the JamJet
 * runtime or Spring Boot starter.
 *
 * @see <a href="https://docs.jamjet.dev/guides/skillsjars">SkillsJars Guide</a>
 */
public final class SkillsJars {

    /** Module version. */
    public static final String VERSION = "0.4.1";

    private SkillsJars() {
        // Utility class
    }
}
