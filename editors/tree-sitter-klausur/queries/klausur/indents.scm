[
  (RequestBody)
  (Connection)
  (UndoBlock)
  (ExprBlock)
  (ExprObject)
  (ExprArray)
  (ArgList)
  (DefParamList)
  (TestParamList)
  (JsonObject)
  (JsonArray)
  (SwitchArmList)
] @indent.begin

[ "}" "]" ")" ] @indent.branch
[ "}" "]" ")" ] @indent.end

; Raw bodies keep their own indentation.
(Fenced) @indent.ignore

[ (comment_block) (comment_block_doc) ] @indent.auto
