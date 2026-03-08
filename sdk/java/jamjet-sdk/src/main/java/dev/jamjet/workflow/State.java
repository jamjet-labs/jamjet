package dev.jamjet.workflow;

import java.lang.annotation.ElementType;
import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.annotation.Target;

/**
 * Marks a class or record as the state type for a JamJet workflow.
 *
 * <p>Use this annotation on the state class to provide metadata to the IR compiler.
 * The state class should be a Java record or a class with Jackson-serializable fields.
 *
 * <pre>{@code
 * @State
 * record MyState(String input, String output) {}
 * }</pre>
 */
@Target(ElementType.TYPE)
@Retention(RetentionPolicy.RUNTIME)
public @interface State {

    /** Optional human-readable description of this state type. */
    String description() default "";
}
