# Brief: `POST /cards/from_node`

Viết lại từ đầu — brief gốc cùng tên (nếu từng tồn tại) không tìm thấy
ở bất kỳ đâu trên máy này: không có trong repo Git (kể cả reflog/mọi
ref), không có file rời nào khớp tên trên toàn bộ filesystem. Bản này
là spec mới, dựa trên suy luận từ các pattern đã có trong codebase
(`generate.rs`, `quiz.rs`, `ks_client.rs`) và 3 quyết định đã được bên
điều phối xác nhận trực tiếp (đánh dấu **[ĐÃ CHỐT]** bên dưới). Phần
còn mơ hồ được liệt kê rõ ở cuối — không tự đoán.

## Mục đích

Khép chiều dữ liệu còn thiếu: Mnemosyne → KS đã có (`/socratic/{id}/end`
gửi transcript sang KS). Chiều ngược lại — biến một khái niệm đã được
Knowledge Store chấp nhận (`ks.nodes`, sau bước `ks.cli accept`) thành
một flashcard ôn tập spaced-repetition trong Mnemosyne — hiện chưa có.
`POST /cards/from_node` lấp chỗ đó: cho 1 `node_id` cụ thể, sinh đúng
1 card gắn vào một `study_set` có sẵn.

## Request/Response

```
POST /cards/from_node
{
  "study_set_id": "<uuid>",   // bắt buộc, set phải tồn tại (FK NOT NULL như mọi card khác)
  "node_id": "<uuid>"         // bắt buộc, id trong ks.nodes
}
```

```
201 Created
{
  "id": "<uuid>",
  "set_id": "<uuid>",
  "question": "...",
  "answer": "...",
  "source": "knowledge_store",
  "source_node_id": "<uuid>",
  "created_at": "...",
  "tokens_used": 123
}
```

Lỗi: `404` nếu `study_set_id` không tồn tại; `503` nếu KS chưa cấu
hình (`KS_HTTP_TOKEN` rỗng, giống pattern đã dùng ở `quiz.rs`); `404`
nếu `node_id` không khớp node nào; `502` nếu LLM trả non-JSON hoặc bị
cắt giữa chừng (`finish_reason`, xem mục vá lỗi bên dưới).

## **[ĐÃ CHỐT]** 1 — Selection model: một `node_id` cụ thể

Không phải batch kiểu `subject_filter` + `count` như
`quiz.rs::generate_quiz`. Mỗi lần gọi ứng với đúng 1 node, đúng như
tên endpoint gợi ý.

**Hệ quả kỹ thuật cần lưu ý**: KS hiện **không có** `GET /nodes/{id}`
— chỉ có `GET /nodes` (list, filter `subject`/`source_module`/`limit`).
Để lấy 1 node theo id, Mnemosyne phải gọi `GET /nodes` (không filter,
hoặc filter theo subject nếu client biết trước) rồi lọc phía client
theo `id`. Cách này đúng về mặt chức năng nhưng kém hiệu quả nếu số
node lớn — **cần bên điều phối xác nhận**: chấp nhận cách này tạm
thời, hay nên xin KS thêm `GET /nodes/{id}` trước khi merge?
`ks_client::KsClient` cần thêm 1 hàm `get_node(id)` gọi `get_nodes(None)`
rồi tự lọc — không sửa gì phía KS trong phạm vi brief này.

## **[ĐÃ CHỐT]** 2 — Sinh bằng LLM, không phải template tĩnh

Gọi DeepSeek diễn đạt `title` + `summary` của node thành 1 cặp
question/answer tự nhiên (không phải chép nguyên `summary` làm đáp
án). Prompt gần với "elaboration" style của `generate.rs`
(`build_elaboration_prompt`) hơn là "recall" ngắn — vì nguồn là 1 khái
niệm đã được dạy qua Socratic, hợp với câu hỏi hiểu sâu hơn là factoid
ngắn.

### Bắt buộc áp đủ 5 biện pháp chống bug đã học — và một khoảng trống có thật cần vá trước

- **`finish_reason` (bug #2)**: `LLMResponse` (`llm_provider.rs`) và
  `DeepSeekResponse`/`DeepSeekChoice` (`deepseek.rs`) **hiện KHÔNG đọc
  `finish_reason`** — chỉ lấy `content` + `usage.total_tokens`. Đây
  không phải lỗi riêng của endpoint mới; đây là khoảng trống có thật
  trong toàn bộ `LLMProvider` hiện tại, ảnh hưởng mọi call site
  (`generate.rs`, `socratic.rs`, `feynman.rs`, `quiz.rs`) — tất cả
  đang có nguy cơ y hệt bug đã ghi nhận: `content=''` do bị cắt giữa
  chừng vẫn trôi xuống parser mà không ai biết. **Đề xuất**: vá 1 lần
  cho toàn hệ thống (thêm `finish_reason: String` vào `LLMResponse`,
  đọc `choices[0].finish_reason` thật từ DeepSeek, raise `LLMError`
  loại mới khi `finish_reason == "length"`) — không vá cục bộ chỉ cho
  `from_node`. Đây là việc lớn hơn phạm vi 1 endpoint, cần xác nhận
  có làm chung đợt này hay tách riêng.
- Parse JSON: tái dùng đúng pattern `parse_*_response` (thử raw, rồi
  strip fence ```` ```json ````) đã có ở 3 module kia — không viết lại
  từ đầu.
- Không nuốt lỗi nghiệp vụ chính bằng `let _ = ...` — chỉ áp dụng cho
  lỗi ghi `ai_interactions` log phụ, đúng pattern hiện có.
- Không cache: Rust backend hiện không có cache nào, nên không phát
  sinh bug #1/#4. **Không nên thêm cache** cho endpoint này (gọi lại
  cùng `node_id` vẫn nên gọi LLM lại — xem mục idempotency).
- HTTP 200 + body lỗi: áp dụng cho `KsClient::get_nodes` (đã có sẵn,
  phân biệt `Unreachable`/`Http`/`Parse`/`KsDbUnavailable`) — tái dùng
  nguyên, không cần thêm gì.

## **[ĐÃ CHỐT]** 3 — Migration 0005: thêm cột truy nguyên vào `cards`

```sql
ALTER TABLE cards
  ADD COLUMN source TEXT NOT NULL DEFAULT 'manual'
    CHECK (source IN ('manual', 'topic', 'knowledge_store')),
  ADD COLUMN source_node_id UUID;  -- không FK — ks.nodes ở DB khác, giống quiz_questions.source_node_id

ALTER TABLE cards
  ADD CONSTRAINT cards_node_id_iff_knowledge_store
    CHECK ((source = 'knowledge_store') = (source_node_id IS NOT NULL));
```

Cùng nguyên tắc đã dùng cho `quiz_questions`: không FK xuyên database,
CHECK ràng buộc `source_node_id` chỉ non-null khi `source='knowledge_store'`.

Thêm `'card_from_node_generation'` vào enum `ai_interaction_type`
(giống cách `quiz_generation` đã được thêm ở migration 0004).

## Việc cần làm nếu được duyệt

1. Migration 0005 (schema trên) + cập nhật `backend/sql/schema.sql`.
2. Vá `finish_reason` cho `LLMProvider`/`DeepSeekClient` — áp dụng
   toàn hệ thống, không chỉ endpoint mới (xem mục 2).
3. `ks_client.rs`: thêm `get_node(id) -> Result<Option<KsNode>, KsError>`
   (gọi `get_nodes(None)`, lọc client-side).
4. Handler mới (`handlers/cards.rs` hoặc file riêng
   `handlers/cards_from_node.rs`) + đăng ký trong `main.rs`.
5. Test: unit test cho parse/validate dùng fake provider (bao gồm mô
   phỏng `finish_reason="length"` — đúng bài học đã ghi), **và** smoke
   test qua API thật trước khi coi xong — có sẵn 2 node thật để test
   ngay (`5248a55b-...` "Định luật II Newton", `8bea853e-...` "Sự
   khác biệt giữa gia tốc và vận tốc", từ phiên verify Socratic trước
   đó).

## Còn mơ hồ — CẦN bên điều phối quyết định trước khi code, không tự đoán

1. **Idempotency**: gọi lại `POST /cards/from_node` với cùng `node_id`
   trong cùng `study_set` — cho phép tạo thêm 1 card trùng lặp (mỗi
   lần LLM diễn đạt khác do non-determinism, coi là biến thể hợp lệ),
   hay chặn bằng `UNIQUE (set_id, source_node_id)` +
   `ON CONFLICT DO NOTHING` trả về card đã có? Ảnh hưởng trực tiếp
   thiết kế migration 0005 ở trên (cần thêm UNIQUE constraint nếu chọn
   chặn).
2. **`generate_cards` có cần đồng bộ theo không?** `POST
   /study_sets/{set_id}/generate_cards` (sinh card từ topic tự do)
   hiện KHÔNG set cột `source` khi insert — nếu để nguyên, mọi card cũ
   tạo qua đường đó sẽ mang default `'manual'`, vốn dùng cho card tạo
   thủ công qua `POST /cards`, dữ liệu sẽ sai lệch (không phân biệt
   được "gõ tay" với "AI sinh từ topic tự do"). Có sửa luôn
   `generate.rs` để gắn `source='topic'` trong đợt này không, hay để
   sau?
3. **`GET /nodes/{id}` phía KS**: đã nêu ở mục 1 — chấp nhận lọc
   client-side tạm thời hay cần xin route mới trước khi merge
   `from_node`?
