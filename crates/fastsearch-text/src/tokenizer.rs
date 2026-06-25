//! 分词器：默认（Tantivy 内置，英文/unicode）+ 中文 jieba。
//!
//! jieba 分词器把 jieba-rs 的切词结果适配成 Tantivy `Tokenizer`：按原文字节偏移
//! 输出 token（便于高亮），小写化（CJK 无副作用），跳过纯空白。lindera/icu 列为
//! 后续迭代。

use std::sync::Arc;
use tantivy::tokenizer::{Token, TokenStream, Tokenizer};

/// 中文分词器（jieba 词典 + HMM）。
#[derive(Clone)]
pub struct JiebaTokenizer {
    jieba: Arc<jieba_rs::Jieba>,
}

impl JiebaTokenizer {
    pub fn new() -> Self {
        JiebaTokenizer {
            jieba: Arc::new(jieba_rs::Jieba::new()),
        }
    }
}

impl Default for JiebaTokenizer {
    fn default() -> Self {
        Self::new()
    }
}

/// 预切好的 token 流。`index` 指向"下一个待返回"的位置，advance 后 token()
/// 取 `index-1`（Tantivy 标准游标惯例）。
pub struct VecTokenStream {
    tokens: Vec<Token>,
    index: usize,
}

impl TokenStream for VecTokenStream {
    fn advance(&mut self) -> bool {
        if self.index < self.tokens.len() {
            self.index += 1;
            true
        } else {
            false
        }
    }
    fn token(&self) -> &Token {
        &self.tokens[self.index - 1]
    }
    fn token_mut(&mut self) -> &mut Token {
        &mut self.tokens[self.index - 1]
    }
}

impl Tokenizer for JiebaTokenizer {
    type TokenStream<'a> = VecTokenStream;

    fn token_stream<'a>(&'a mut self, text: &'a str) -> VecTokenStream {
        // jieba 0.10 的 Token 自带 byte_start/byte_end，直接用作字节偏移（便于高亮）。
        let words = self.jieba.cut(text, true);
        let mut tokens = Vec::with_capacity(words.len());
        let mut pos = 0usize;
        for w in words {
            if w.word.trim().is_empty() {
                continue;
            }
            tokens.push(Token {
                offset_from: w.byte_start,
                offset_to: w.byte_end,
                position: pos,
                text: w.word.to_lowercase(),
                position_length: 1,
            });
            pos += 1;
        }
        VecTokenStream { tokens, index: 0 }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collect(text: &str) -> Vec<String> {
        let mut tk = JiebaTokenizer::new();
        let mut stream = tk.token_stream(text);
        let mut out = vec![];
        while stream.advance() {
            out.push(stream.token().text.clone());
        }
        out
    }

    #[test]
    fn segments_chinese() {
        let toks = collect("我爱北京天安门");
        assert!(toks.contains(&"北京".to_string()));
        assert!(toks.contains(&"天安门".to_string()));
    }

    #[test]
    fn lowercases_and_skips_space() {
        let toks = collect("Hello 世界");
        assert!(toks.contains(&"hello".to_string()));
        assert!(toks.contains(&"世界".to_string()));
        assert!(!toks.iter().any(|t| t.trim().is_empty()));
    }
}
