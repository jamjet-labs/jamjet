package dev.jamjet.loan.config;

import dev.jamjet.cloud.agentboundary.ActionReceiptEmitter;
import dev.jamjet.cloud.spring.ActionReceiptAdvisor;
import org.springframework.ai.chat.client.ChatClient;
import org.springframework.ai.chat.model.ChatModel;
import org.springframework.boot.autoconfigure.condition.ConditionalOnBean;
import org.springframework.boot.autoconfigure.condition.ConditionalOnProperty;
import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;
import org.springframework.core.Ordered;
import org.springframework.core.env.Environment;

/**
 * OFF-BY-DEFAULT configuration for the real LLM path.
 *
 * <p>When you add a model starter (e.g. {@code spring-ai-starter-model-openai}),
 * set {@code loan.llm.enabled=true} in {@code application.properties} and supply
 * the relevant API key (e.g. {@code OPENAI_API_KEY}), every tool call the model
 * makes emits an AgentBoundary receipt automatically via {@link ActionReceiptAdvisor}.
 *
 * <p>By default (no property set, no model starter on the classpath) this config
 * is completely inert: {@code @ConditionalOnProperty} prevents the class from being
 * processed, and {@code @ConditionalOnBean(ChatModel.class)} on the bean method adds
 * a second guard so that even if the property is set, the bean is only created when a
 * real {@link ChatModel} implementation is present. The default context load and all
 * deterministic demo tests (Part A) are entirely unaffected.
 */
@Configuration
@ConditionalOnProperty(prefix = "loan.llm", name = "enabled", havingValue = "true")
public class ChatClientConfig {

    /**
     * Build a {@link ChatClient} wired with {@link ActionReceiptAdvisor} so that
     * every model tool call emits a signed AgentBoundary receipt.
     *
     * <p>This bean is only created when a {@link ChatModel} bean is present on the
     * context (i.e. a model starter has been added to the classpath and configured).
     */
    @Bean
    @ConditionalOnBean(ChatModel.class)
    public ChatClient chatClient(
            ChatModel chatModel,
            Environment environment,
            ActionReceiptEmitter emitter) {
        return ChatClient.builder(chatModel)
                .defaultAdvisors(new ActionReceiptAdvisor(environment, emitter, Ordered.LOWEST_PRECEDENCE))
                .build();
    }
}
