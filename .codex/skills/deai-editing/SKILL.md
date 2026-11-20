---
name: deai-editing
description: Edit existing prose to remove formulaic, AI-sounding phrasing while preserving its facts, intent, tone, and required structure. Use for requests to humanize or de-AI manuals, articles, product copy, and other drafts; not for writing new content from scratch.
---

# Natural prose editing

Edit the text the user provides or identifies. Keep its meaning, factual claims, position, level of formality, audience, and necessary qualifications. Do not add unsupported examples, stronger promises, new conclusions, or a different point of view. If a claim seems wrong, flag it separately only when the user permits commentary; do not silently change the fact while doing a style edit.

Remove empty openings, generic transitions, repeated conclusions, symmetrical phrasing, forced three-part lists, and abstract praise that gives the reader no useful information. Replace vague actions with the specific person, object, button, number, or situation already supported by the source. Shorten sentences that carry several ideas; keep a longer sentence when it reads naturally and preserves an important condition. Let sentence and paragraph lengths vary.

Avoid stock phrases such as "firstly/secondly/finally," "in conclusion," "it is worth noting," and their equivalents in the text's language. In Chinese, watch for "首先/其次/最后", "值得注意的是", "综上所述", "不仅……而且", "既……又", "赋能", "抓手", "闭环", "维度", and "底层逻辑". Do not ban a word when it has a precise, necessary meaning. Do not add slang, errors, swearing, or theatrical spoken fillers to simulate a human voice.

Keep formal documents formal and conversational writing conversational. Retain meaningful uncertainty and the writer's own voice. Do not polish every paragraph into the same opening, explanation, and concluding sentence. One paragraph should usually handle one point. Keep lists, headings, warnings, links, captions, and screenshot references when readers need them to complete a task; do not flatten a useful manual into prose merely to avoid a list.

For help content, preserve the order of user tasks, exact visible control names, prerequisites, expected results, and warnings. Do not invent interface details. If a screenshot or interface detail is missing, keep the existing placeholder or mark it as pending rather than guessing. When parallel language versions exist and the request covers the whole document set, edit each version for natural expression in that language; do not force sentence-by-sentence symmetry.

Before finishing, compare the revision with the source for dropped facts, changed conditions, and invented details. Read it once for boilerplate, excessive balance, repeated endings, empty adjectives, and fake colloquialism. Revise any passage that still has those problems. If editing files, preserve markup and verify that links, image references, and the document build still work where relevant.

Follow the user's requested output format. When asked for only the revised body, provide no preface, change list, explanation, or extra title. When editing an existing file, write the revised text to that file; do not replace the result with a suggested draft in chat.
