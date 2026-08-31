;; song_coverage.bb — 从 resources-route 扫描输出聚合"歌曲 ↔ 歌词 bundle"覆盖清单。
;;
;; 输入:
;;   build/experiments/resources-route/songs.tsv      (id \t 曲名,来自 mlMusic_Name)
;;   build/experiments/resources-route/title_map.txt  (扫描器全量输出)
;; 输出:
;;   build/experiments/resources-route/song_coverage.tsv  (id \t 曲名 \t 状态 \t bundle路径)
;;
;; 用法: bb song_coverage.bb   (在 experiments/resources-route/ 下,或改路径参数)

(require '[clojure.string :as str])

(def ^:dynamic *base* "build/experiments/resources-route")

(defn- parse-songs [path]
  (into {}
        (for [l (str/split-lines (slurp path))
              :let [[id t] (str/split l #"\t" 2)]
              :when (and id t)]
          [id t])))

(defn- parse-scan [path]
  (loop [[l & ls] (str/split-lines (slurp path))
         cur nil acc (sorted-map)]
    (cond
      (nil? l) acc
      (str/starts-with? l "=== ")
      (recur ls (subs l 4) (assoc-in acc [(subs l 4) :exists] true))
      (str/starts-with? l "   song ")
      (let [id (second (str/split (first (str/split (str/trim l) #"\t" 2)) #" "))]
        (recur ls cur (update-in acc [cur :songs] (fnil conj #{}) id)))
      (str/starts-with? l "   mid ")
      (let [[_ mid cnt] (str/split (str/trim l) #"\s+")]
        (recur ls cur (update-in acc [cur :mids] (fnil conj {})
                                 [(parse-long mid) (parse-long (subs cnt 1))])))
      :else (recur ls cur acc))))

(defn- assign [bundle]
  (let [{:keys [songs mids]} bundle
        strong (filter (comp #(>= % 2) second) mids)
        max-c (when (seq strong) (apply max (map second strong)))
        cands (when max-c (map first (filter #(= max-c (second %)) strong)))
        top (when (= 1 (count cands)) (first cands))
        chosen (cond
                 (= 1 (count songs)) (first songs)
                 (pos? (count strong)) (some-> top str)
                 :else nil)]
    chosen))

(let [songs (parse-songs (str *base* "/songs.tsv"))
      scan (parse-scan (str *base* "/title_map.txt"))
      by-song (into {}
                    (for [[b bundle] scan
                          :let [c (assign bundle)]
                          :when c]
                      [c b]))
      found (count (distinct (vals by-song)))]
  (spit (str *base* "/song_coverage.tsv")
        (str/join "\n"
                  (for [[id t] (sort-by (comp parse-long key) songs)]
                    (let [b (get by-song id)]
                      (str id "\t" t "\t" (if b "FOUND" "NO-BUNDLE") "\t" (or b ""))))))
  (println "lyric bundles:" (count scan)
           "| identified:" (count by-song)
           "| songs found:" found "/" (count songs))
  (let [{no-mid :no-mid ambiguous :ambiguous} (group-by
                                                (fn [[_ {:keys [songs mids]}]]
                                                  (let [strong (->> mids
                                                                    (filter (comp #(>= % 2) second))
                                                                    (map first))]
                                                    (cond
                                                      (= 1 (count songs)) :matched
                                                      (= 1 (count strong)) :matched
                                                      (pos? (count strong)) :ambiguous
                                                      :else :no-mid)))
                                                scan)]
    (println "unidentified bundles:" (+ (count no-mid) (count ambiguous))
             "(no-id:" (count no-mid) "ambiguous:" (count ambiguous) ")")))
