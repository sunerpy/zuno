# Variables and substitution

Two tokens are expanded in configuration text before it is parsed: `{env:NAME}` reads an
environment variable, and `{file:PATH}` reads a file. Together they are how a secret or a
long value stays out of the JSON document.

```json
{
  "provider": {
    "myopenai": {
      "options": {
        "baseURL": "{env:MYOPENAI_BASE_URL}"
      }
    }
  }
}
```

## Substitution happens on text, not on values

This is the rule that explains every edge case below. Expansion runs over the file's bytes
and the result is handed to the JSONC parser. A token is therefore expanded wherever it
appears: inside a key, inside a string, spanning a quote, even spanning a line break.

The practical consequence is that a substituted value must be valid where it lands. A value
containing a double quote inserted into a JSON string will produce a parse error at the
document level rather than a validation error on that field.

## `{env:NAME}`

One unconditional pass over the whole text.

| Text | Result |
| --- | --- |
| `{env:PRESENT}` | The variable's value |
| `{env:ABSENT}` | Empty string. A missing variable is not an error |
| `{env:}` | Unchanged. A name is required, so this is not a token |
| `{env:FOO` | Unchanged. No closing brace, no token |
| `{"a":"{env:FOO"}` | The match runs past the quote to the next `}` |

An absent variable expanding to empty is worth internalizing: a typo in the variable name
produces an empty value, not a diagnostic. Verify with `zuno debug config` rather than
assuming.

A `{env:...}` token inside a `//` comment line **is** substituted, because the environment
pass does not recognize comments.

## `{file:PATH}`

The file pass runs over the already-expanded text, so an environment variable whose value
contains a `{file:...}` token does get that file read. A file body is not rescanned for
either token.

Path resolution has three shapes:

| Spec | Resolution |
| --- | --- |
| `~/x` | Joined onto the home directory, normalized |
| An absolute path | Used verbatim, including `..` and `//`, resolved by the kernel |
| Anything else | Resolved against the **config file's directory**, not the process working directory |

Content is trimmed, and the result is escaped for insertion into a JSON string. A file that
is not valid UTF-8 yields replacement characters rather than failing.

```json
{
  "provider": {
    "myopenai": {
      "options": {
        "apiKey": "{file:~/.secrets/myopenai.key}"
      }
    }
  }
}
```

### Missing files

For `zuno.json`, an unreadable `{file:...}` target fails the load and the error names both
the token and the resolved path. That covers every read failure, not only absence: a token
pointing at a directory fails the same way.

For `tui.json`, a missing target substitutes nothing and the load continues. The asymmetry
is deliberate. A wrong provider endpoint should stop the process; a missing decorative
value should not cost you an interface.

### The comment-skip rule

The `{file:...}` pass skips a token on a `//` comment line. Stated exactly: take the text
from the start of the token's line up to the token, drop leading whitespace, and skip the
token when what remains begins with `//`.

| Text | Substituted? |
| --- | --- |
| `  // {file:x}` | No |
| `/// {file:x}` | No |
| `{"a":1} // {file:x}` | Yes. A trailing comment is not a comment line |
| `{"a":"// {file:x}"}` | Yes. `//` inside a string value is not a comment |
| `/* {file:x} */` | Yes. Block comments are not recognized |
| `// {file:a} and {file:b}` | No, for both. Every token on the line is skipped |

A skipped token is never read, so a missing file inside a comment cannot fail the load. That
makes commenting out a `{file:...}` line a safe way to disable it.

## Secrets: what to prefer

Order of preference, best first:

1. A provider login credential, stored by `zuno providers login`.
2. An environment variable declared under `provider.<id>.env`, consumed directly.
3. `{file:...}` pointing at a file with restricted permissions.
4. `{env:...}` inline in configuration.
5. A literal `apiKey` string in `zuno.json`.

Amazon Bedrock is the deliberate exception: `AWS_BEARER_TOKEN_BEDROCK`
outranks the stored bearer token, and when neither exists the AWS SDK credential
chain resolves profile, access-key, web-identity, container, and instance-role
credentials.

The last is supported but exposes the secret to configuration backups and source control.
Credential precedence is documented in [Authentication](/config/authentication).

`ZUNO_AUTH_CONTENT` can replace credential reads entirely with a JSON object, which is the
right shape for a managed or ephemeral environment where nothing should be written to disk.

## Verifying an expansion

```sh
zuno debug config
```

This prints the merged configuration after substitution, so it is where to confirm that a
token resolved to what you expected. Be aware that a resolved secret is visible in that
output; treat it accordingly.

## See also

- [Files and precedence](/config/files)
- [Authentication](/config/authentication)
- [Configuration overview](/config/)
- [Configuration reference](/reference/configuration)
