def flags: map(select(test("^-{1,2}[A-Za-z][A-Za-z0-9-]*($|=)")) | sub("=.*"; "="));
def prog: if contains("%") then "?" else (split("/") | last | if test("^[a-z0-9][a-z0-9_.+-]{0,39}$") then . else "?" end) end;
def simple: ((to_entries | map(select(.value | test("^[A-Za-z_][A-Za-z0-9_]*=") | not)) | first.key) // length) as $n
  | (.[:$n] | map(sub("=.*"; "="))) + (.[$n:] | if length == 0 then []
    elif length > 1 and (.[0] | prog | IN("git", "gh", "cargo", "rustup", "go", "npm", "pnpm", "yarn", "bun", "uv", "pip", "nix", "docker", "kubectl", "just", "quest", "token-max"))
      and (.[1] | test("^[a-z][a-z0-9-]{0,30}$")) then [(.[0] | prog), .[1]] + (.[2:] | flags)
    else [.[0] | prog] + (.[1:] | flags) end);
# A command with everything quoted, escaped or substituted masked as `%`, tracking quotes and
# `$(...)` as the shell nests them; a continuation is deleted, a comment dropped, and a heredoc
# ends it, since its body is data.
def mask: reduce (explode[] | [.] | implode) as $c ({o: "", s: ["t"], e: false, p: "", x: false};
  if .x then .
  elif .e then .e = false | if $c == "\n" then . else .o += "%%" | .p = "" end
  else .s[-1] as $t
    | if $t == "c" then (if $c == "\n" then .s |= .[:-1] | .o += "\n" else . end)
      elif $t == "s" then (if $c == "'" then .s |= .[:-1] else . end) | .o += "%"
      elif $c == "\\" then .e = true
      elif $t == "d" then (if $c == "\"" then .s |= .[:-1] elif $c == "(" and .p == "$" then .s += ["u"] elif $c == "`" then .s += ["b"] else . end) | .o += "%"
      elif $t == "b" then (if $c == "`" then .s |= .[:-1] else . end) | .o += "%"
      elif $c == "<" and .p == "<" then .x = true
      elif $c == "#" and (.p | test("^$|[\\s;&|()]")) then .s += ["c"]
      elif $c == "'" then .s += ["s"] | .o += "%"
      elif $c == "\"" then .s += ["d"] | .o += "%"
      elif $c == "`" then .s += ["b"] | .o += "%"
      elif $t == "u" then (if $c == "(" then .s += ["u"] elif $c == ")" then .s |= .[:-1] else . end) | .o += "%"
      elif $c == "(" and .p == "$" then .s += ["u"] | .o += "%"
      else .o += $c end
    | if .e then . else .p = $c end
  end) | .o;
def cmd: if type == "array" then map(tostring)
    | if length == 3 and (.[0] | test("(^|/)(ba|z|da)?sh$")) then .[2] | cmd else simple end
  else [tostring | mask
    | splits("&&|\\|\\||[;|\\n]") | [splits("\\s+") | select(. != "")] | select(length > 0) | simple] | .[:4] end;
def safe($tool): if type == "object" then
    del(.internal_chat_message_metadata_passthrough, .permission_profile, .sandbox_policy, .rate_limits, .encrypted_content, .signature, .betas, .annotations, .logprobs, .text_elements, .parentUuid, .uuid, .promptId)
    # A tool's payload can key its data by text, as AskUserQuestion keys answers by question.
    | if $tool then to_entries | to_entries | map(.key as $i | .value
        | if .key | test("^[A-Za-z_][A-Za-z0-9_]{0,40}$") then . else .key = "<\(.key | length) chars, #\($i)>" end) | from_entries
      else . end
    | with_entries(.key as $k | ($tool or ($k | IN("input", "arguments", "output", "toolUseResult"))) as $in | .value |= (type as $t
      | if $k == "arguments" and $t == "string" then (. as $s | try (fromjson | safe(true)) catch "<\($s | length) chars>")
      elif ($k == "command" or $k == "cmd") and ($t == "string" or $t == "array") then cmd
      elif $t == "string" and ($k | test(if $tool then "^(file_path|path|notebook_path|cwd|workdir)$"
          else "^(type|subtype|role|name|tool|model|effort|status|phase|timestamp|sessionId|session_id|id|call_id|tool_use_id|file_path|path|notebook_path|cwd|workdir|exit_code|is_error|stop_reason|service_tier|speed)$" end)) then .[:200]
      else safe($in) end))
  elif type == "array" then (.[:20] | map(safe($tool))) + (if length > 20 then ["+\(length - 20) more"] else [] end)
  elif type == "string" then "<\(length) chars>"
  else . end;
safe(false)
