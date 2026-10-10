import js from '@eslint/js';
import svelte from 'eslint-plugin-svelte';
import globals from 'globals';
import ts from 'typescript-eslint';

export default ts.config(
    js.configs.recommended,
    ...ts.configs.recommended,
    ...svelte.configs['flat/recommended'],
    ...svelte.configs['flat/prettier'],
    {
        languageOptions: {
            globals: {
                ...globals.browser,
                ...globals.node
            }
        }
    },
    {
        files: ['**/*.svelte'],
        languageOptions: {
            parserOptions: {
                parser: ts.parser
            }
        }
    },
    {
        rules: {
            // Allow catch(err: any) patterns
            '@typescript-eslint/no-explicit-any': 'off',
            // Svelte $props destructuring looks unused to the linter
            '@typescript-eslint/no-unused-vars': [
                'warn',
                {
                    argsIgnorePattern: '^_',
                    varsIgnorePattern: '^\\$|^_'
                }
            ],
            // SvelteKit uses href directly without resolve()
            'svelte/no-navigation-without-resolve': 'off',
            // SvelteDate/SvelteSet not needed for non-reactive usages
            'svelte/prefer-svelte-reactivity': 'off'
        }
    },
    {
        ignores: ['.svelte-kit/**', 'build/**', 'node_modules/**', 'src/lib/bindings.ts']
    }
);
