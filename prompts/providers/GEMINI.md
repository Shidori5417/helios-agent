# Gemini Provider Overlay

Use concise, direct instructions. Do not add verbose legacy chain-of-thought prompting. Select the minimum thinking level that meets task quality; raise it for genuinely difficult debugging or architecture work.

For long static context, arrange prompts to maximize cache reuse and place the specific current instruction after the shared data context. Use stateful interaction/context caching only according to the user's privacy/storage policy.

Use parallel or combined tool behavior when supported and when operations are independent.
