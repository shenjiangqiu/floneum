use crate::{
    download::download, embedding::get_embeddings, exports::plugins::main::definitions::Embedding,
    vector_db::VectorDB, EmbeddingDbId, GptNeoXType, LlamaType, ModelId, ModelType, MptType,
};
use futures_util::stream::StreamExt;
use llm::{
    InferenceFeedback, InferenceRequest, InferenceResponse, InferenceSession, LoadProgress, Model,
    ModelArchitecture,
};
use slab::Slab;
use spinoff::{spinners::Dots2, Spinner};
use std::{convert::Infallible, error::Error, path::PathBuf, time::Instant};
use tokio::{fs::File, io::AsyncWriteExt, runtime::Handle};

#[derive(Default)]
pub struct InferenceSessions {
    sessions: Slab<(Box<dyn Model>, llm::InferenceSession)>,
    vector_dbs: Slab<VectorDB<String>>,
}

impl InferenceSessions {
    pub fn session_get(&self, id: ModelId) -> &(Box<dyn Model>, InferenceSession) {
        self.sessions.get(id.id as usize).unwrap()
    }

    pub fn session_get_mut(&mut self, id: ModelId) -> &mut (Box<dyn Model>, InferenceSession) {
        self.sessions.get_mut(id.id as usize).unwrap()
    }

    pub fn vector_db_get(&self, id: EmbeddingDbId) -> &VectorDB<String> {
        self.vector_dbs.get(id.id as usize).unwrap()
    }

    #[allow(unused)]
    pub fn vector_db_get_mut(&mut self, id: EmbeddingDbId) -> &mut VectorDB<String> {
        self.vector_dbs.get_mut(id.id as usize).unwrap()
    }

    pub fn create(&mut self, ty: ModelType) -> ModelId {
        let model = download(ty);
        let session = model.start_session(Default::default());
        ModelId {
            id: self.sessions.insert((model, session)) as u32,
        }
    }

    pub fn remove(&mut self, id: ModelId) {
        self.sessions.remove(id.id as usize);
    }

    pub fn infer(
        &mut self,
        id: ModelId,
        prompt: String,
        max_tokens: Option<u32>,
        stop_on: Option<String>,
    ) -> String {
        let (model, session) = self.session_get_mut(id);

        let mut rng = rand::thread_rng();
        let mut buf = String::new();
        let request = InferenceRequest {
            prompt: (&prompt).into(),
            parameters: &Default::default(),
            play_back_previous_tokens: false,
            maximum_token_count: max_tokens.map(|x| x as usize),
        };

        session
            .infer(
                model.as_ref(),
                &mut rng,
                &request,
                &mut Default::default(),
                inference_callback(stop_on, &mut buf),
            )
            .unwrap_or_else(|e| panic!("{e}"));

        buf
    }

    pub fn get_embedding(&self, id: ModelId, text: &str) -> Embedding {
        let (model, _session) = self.session_get(id);
        let inference_parameters = llm::InferenceParameters::default();
        get_embeddings(model.as_ref(), &inference_parameters, text)
    }

    pub fn create_db(
        &mut self,
        embedding: Vec<Embedding>,
        documents: Vec<String>,
    ) -> EmbeddingDbId {
        let idx = self.vector_dbs.insert(VectorDB::new(embedding, documents));

        EmbeddingDbId { id: idx as u32 }
    }

    pub fn get_closest(&self, id: EmbeddingDbId, embedding: Embedding, n: usize) -> Vec<String> {
        self.vector_db_get(id).get_closest(embedding, n)
    }

    pub fn get_within(
        &self,
        id: EmbeddingDbId,
        embedding: Embedding,
        distance: f32,
    ) -> Vec<String> {
        self.vector_db_get(id).get_within(embedding, distance)
    }
}

fn inference_callback(
    stop_sequence: Option<String>,
    buf: &mut String,
) -> impl FnMut(InferenceResponse) -> Result<InferenceFeedback, Infallible> + '_ {
    move |resp| match resp {
        InferenceResponse::InferredToken(t) => {
            let mut reverse_buf = buf.clone();
            reverse_buf.push_str(t.as_str());
            if let Some(stop_sequence) = &stop_sequence {
                if stop_sequence.as_str().eq(reverse_buf.as_str()) {
                    return Ok(InferenceFeedback::Halt);
                }
            }
            buf.push_str(t.as_str());

            Ok(InferenceFeedback::Continue)
        }
        InferenceResponse::EotToken => Ok(InferenceFeedback::Halt),
        _ => Ok(InferenceFeedback::Continue),
    }
}
